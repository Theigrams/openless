//! Bounded AX snapshot of the visible foreground window outside the input field.

use std::ffi::c_void;
use std::time::{Duration, Instant};

use core_foundation::array::{CFArray, CFArrayRef};
use core_foundation::base::{CFRange, TCFType};
use core_foundation::string::{CFString, CFStringGetCharacters};
use core_foundation::url::{CFURLRef, CFURL};
use core_graphics::geometry::{CGPoint, CGRect, CGSize};
use openless_core::host_document::{
    window_text_meets_screenshot_threshold, WindowCapture, WindowCaptureMode, WindowContext,
    WindowScreenshot, WINDOW_CONTEXT_PROMPT_CHARS, WINDOW_SCREENSHOT_MAX_BYTES,
};

use super::{
    cfstring_to_rust, copy_attr, copy_element_attr, copy_string_attr,
    focused_element_passing_the_gate_with_timeout, AXUIElementCreateApplication, AXUIElementGetPid,
    AXUIElementSetMessagingTimeout, AXValueGetValue, AxError, AxUiElementRef, CFEqual, CFGetTypeID,
    CFRelease, CFStringGetLength, CFStringGetTypeID, CFStringRef, CFTypeId, GateInputs,
    GatedElement, SendableElement, AX_ERROR_SUCCESS,
};
use crate::host_document::{BlockReason, HostDocumentReadResult, HostDocumentStatus};
use crate::selection::{self, SelectionInsertionTarget};

const CAPTURE_BUDGET: Duration = Duration::from_millis(600);
const MESSAGE_TIMEOUT: f32 = 0.035;
const TEXT_BUDGET: Duration = Duration::from_millis(500);
const MAX_DEPTH: usize = 50;
const MAX_WEB_AREA_DEPTH: usize = 10;
const BROWSER_BUNDLE_PREFIXES: &[&str] = &[
    "com.google.Chrome",
    "com.apple.Safari",
    "com.microsoft.edgemac",
    "org.mozilla.firefox",
    "org.mozilla.nightly",
    "com.operasoftware.Opera",
    "org.chromium.Chromium",
    "com.openai.atlas",
    "company.thebrowser.Browser",
    "ai.perplexity.comet",
    "com.brave.Browser",
    "com.vivaldi.Vivaldi",
    "com.duckduckgo.macos.browser",
    "com.kagi.kagimacOS",
];
const AX_VALUE_CGPOINT: i32 = 1;
const AX_VALUE_CGSIZE: i32 = 2;
static CAPTURE_SLOT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXUIElementGetTypeID() -> CFTypeId;
    fn AXValueGetTypeID() -> CFTypeId;
    fn AXUIElementCopyMultipleAttributeValues(
        element: AxUiElementRef,
        attributes: CFArrayRef,
        options: u32,
        values: *mut CFArrayRef,
    ) -> AxError;
}

const SCREENSHOT_BUDGET: Duration = Duration::from_millis(1200);
static SCREENSHOT_SLOT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

type ScreenshotResult = (
    tokio::sync::oneshot::Sender<Option<WindowScreenshot>>,
    tokio::sync::SemaphorePermit<'static>,
);

extern "C" {
    fn openless_capture_window_png(
        pid: i32,
        bounds: CGRect,
        max_bytes: usize,
        context: *mut c_void,
        callback: extern "C" fn(*mut c_void, *const u8, usize),
    );
}

extern "C" fn receive_screenshot(context: *mut c_void, bytes: *const u8, length: usize) {
    let (sender, _permit) = unsafe { *Box::from_raw(context.cast::<ScreenshotResult>()) };
    let image = if bytes.is_null() || length > WINDOW_SCREENSHOT_MAX_BYTES || sender.is_closed() {
        None
    } else {
        WindowScreenshot::from_png(unsafe { std::slice::from_raw_parts(bytes, length) })
    };
    let _ = sender.send(image);
}

pub(crate) async fn read_window_context(
    target: SelectionInsertionTarget,
    mode: WindowCaptureMode,
    screenshot_threshold: u32,
) -> Option<WindowCapture> {
    if mode == WindowCaptureMode::Text {
        return capture_for_target(target)
            .await
            .ok()
            .map(WindowCapture::Text);
    }

    let permit = CAPTURE_SLOT.try_acquire().ok()?;
    let start_target = target.clone();
    let deadline = Instant::now() + CAPTURE_BUDGET;
    let read = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let focus = unsafe { focused_window(&start_target, deadline) }?;
        let text = if mode == WindowCaptureMode::TextWithScreenshotFallback {
            match unsafe { capture(&start_target, &focus, deadline) } {
                Ok(text) => Some(text),
                Err(Unavailable("no_visible_text") | Timeout) => None,
                Err(error) => return Err(error),
            }
        } else {
            None
        };
        Ok((focus, text))
    });
    let (focus, text) = tokio::time::timeout(CAPTURE_BUDGET + Duration::from_millis(80), read)
        .await
        .ok()?
        .ok()?
        .ok()?;

    if text.as_ref().is_some_and(|text| {
        window_text_meets_screenshot_threshold(&text.visible_text, screenshot_threshold)
    }) {
        return text.map(WindowCapture::Text);
    }
    capture_screenshot(target, focus)
        .await
        .map(WindowCapture::Screenshot)
        .or_else(|| text.map(WindowCapture::Text))
}

async fn capture_screenshot(
    target: SelectionInsertionTarget,
    focus: FocusedWindow,
) -> Option<WindowScreenshot> {
    let permit = SCREENSHOT_SLOT.try_acquire().ok()?;
    let (sender, receiver) = tokio::sync::oneshot::channel::<Option<WindowScreenshot>>();
    let task = tokio::task::spawn_blocking(move || {
        if unsafe { !focus.screenshot_is_current(&target) } {
            return None;
        }
        let context = Box::into_raw(Box::<ScreenshotResult>::new((sender, permit))).cast();
        unsafe {
            openless_capture_window_png(
                focus.pid,
                focus.bounds,
                WINDOW_SCREENSHOT_MAX_BYTES,
                context,
                receive_screenshot,
            );
        }
        Some((target, focus))
    });
    tokio::time::timeout(SCREENSHOT_BUDGET, async {
        let (target, focus) = task.await.ok()??;
        let screenshot = receiver.await.ok()??;
        tokio::task::spawn_blocking(move || {
            unsafe { focus.screenshot_is_current(&target) }.then_some(screenshot)
        })
        .await
        .ok()?
    })
    .await
    .ok()?
}

pub(crate) async fn probe_window_context() -> HostDocumentReadResult<WindowContext> {
    let started = Instant::now();
    let target = selection::capture_selection_insertion_target();
    let app_name = selection::front_app_for_target(&target);
    let bundle_id = selection::bundle_id_for_target(&target);
    let mut result = match capture_for_target(target).await {
        Ok(window) => HostDocumentReadResult {
            window: Some(window),
            ..HostDocumentReadResult::new(HostDocumentStatus::Ok, None)
        },
        Err(CaptureError::Blocked(reason)) => {
            HostDocumentReadResult::new(HostDocumentStatus::Blocked, Some(reason.as_str().into()))
        }
        Err(CaptureError::Unavailable(reason)) => {
            HostDocumentReadResult::new(HostDocumentStatus::Unavailable, Some(reason.into()))
        }
        Err(CaptureError::Timeout) => {
            HostDocumentReadResult::new(HostDocumentStatus::Timeout, None)
        }
    };
    result.app_name = app_name;
    result.bundle_id = bundle_id;
    result.elapsed_ms = started.elapsed().as_millis() as u64;
    result
}

enum CaptureError {
    Blocked(BlockReason),
    Unavailable(&'static str),
    Timeout,
}

use CaptureError::{Blocked, Timeout, Unavailable};

async fn capture_for_target(
    target: SelectionInsertionTarget,
) -> Result<WindowContext, CaptureError> {
    let permit = CAPTURE_SLOT
        .try_acquire()
        .map_err(|_| Unavailable("capture_busy"))?;
    let deadline = Instant::now() + CAPTURE_BUDGET;
    let task = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        unsafe {
            let focus = focused_window(&target, deadline)?;
            capture(&target, &focus, deadline)
        }
    });
    tokio::time::timeout(CAPTURE_BUDGET, task)
        .await
        .map_err(|_| Timeout)?
        .map_err(|_| Unavailable("capture_failed"))?
}

struct FocusedWindow {
    pid: i32,
    app: SendableElement,
    focused: SendableElement,
    window: SendableElement,
    bounds: CGRect,
}

impl FocusedWindow {
    unsafe fn is_current(&self, target: &SelectionInsertionTarget) -> bool {
        selection::selection_insertion_target_is_current(target)
            && !crate::unicode_keystroke::is_secure_input_enabled()
            && owned_element_attr(&self.app, b"AXFocusedWindow\0")
                .is_some_and(|current| same_element(&self.window, &current))
            && owned_element_attr(&self.app, b"AXFocusedUIElement\0")
                .is_some_and(|current| same_element(&self.focused, &current))
    }

    unsafe fn screenshot_is_current(&self, target: &SelectionInsertionTarget) -> bool {
        self.is_current(target)
            && frame(&self.window).is_some_and(|bounds| {
                bounds.origin.x == self.bounds.origin.x
                    && bounds.origin.y == self.bounds.origin.y
                    && bounds.size.width == self.bounds.size.width
                    && bounds.size.height == self.bounds.size.height
            })
    }
}

unsafe fn focused_window(
    target: &SelectionInsertionTarget,
    deadline: Instant,
) -> Result<FocusedWindow, CaptureError> {
    let pid = selection::current_front_app_pid().ok_or(Unavailable("no_foreground_app"))?;
    if Instant::now() >= deadline {
        return Err(Timeout);
    }
    if !selection::selection_insertion_target_is_current(target) {
        return Err(Unavailable("foreground_app_changed"));
    }
    let bundle = selection::bundle_id_for_target(target).ok_or(Unavailable("unknown_app"))?;
    if pid as u32 == std::process::id() {
        return Err(Unavailable("openless_is_foreground"));
    }
    let gate = GateInputs {
        secure_input: crate::unicode_keystroke::is_secure_input_enabled(),
        bundle_id: Some(bundle),
        role: None,
        subrole: None,
    };
    let focused = match focused_element_passing_the_gate_with_timeout(gate, MESSAGE_TIMEOUT) {
        GatedElement::Ready(focused) => focused,
        GatedElement::Blocked(reason) => return Err(Blocked(reason)),
        GatedElement::Unavailable(reason) => return Err(Unavailable(reason)),
    };
    let focused = SendableElement(focused as usize);
    if !owned_by(&focused, pid) || !selection::selection_insertion_target_is_current(target) {
        return Err(Unavailable("foreground_app_changed"));
    }
    let app = AXUIElementCreateApplication(pid);
    if app.is_null() {
        return Err(Unavailable("no_accessible_app"));
    }
    let app = SendableElement(app as usize);
    AXUIElementSetMessagingTimeout(app.as_ref(), MESSAGE_TIMEOUT);
    let window =
        owned_element_attr(&app, b"AXFocusedWindow\0").ok_or(Unavailable("no_focused_window"))?;
    let input_window =
        owned_element_attr(&focused, b"AXWindow\0").ok_or(Unavailable("no_input_window"))?;
    if !same_element(&window, &input_window) || !owned_by(&window, pid) {
        return Err(Unavailable("input_window_mismatch"));
    }
    let window_rect = frame(&window).ok_or(Unavailable("no_window_bounds"))?;
    Ok(FocusedWindow {
        pid,
        app,
        focused,
        window,
        bounds: window_rect,
    })
}

unsafe fn capture(
    target: &SelectionInsertionTarget,
    focus: &FocusedWindow,
    deadline: Instant,
) -> Result<WindowContext, CaptureError> {
    let domain = page_domain(&focus.focused, focus.pid, deadline);
    let title = bounded_text(&focus.window, b"AXTitle\0", 256);
    let browser = selection::bundle_id_for_target(target).is_some_and(|bundle| {
        BROWSER_BUNDLE_PREFIXES
            .iter()
            .any(|prefix| bundle.starts_with(prefix))
    });
    // Leave the rest of the request budget for the final focus and secure-input checks.
    let walk_deadline = deadline - (CAPTURE_BUDGET - TEXT_BUDGET);
    let visible_text = visible_text(focus, browser, walk_deadline);

    if visible_text.is_empty() {
        return Err(Unavailable("no_visible_text"));
    }
    if Instant::now() >= deadline {
        return Err(Timeout);
    }
    if crate::unicode_keystroke::is_secure_input_enabled() {
        return Err(Blocked(BlockReason::SecureInput));
    }
    if !focus.is_current(target) {
        return Err(Unavailable("focus_changed"));
    }
    Ok(WindowContext {
        title,
        domain,
        visible_text,
    })
}

unsafe fn visible_text(focus: &FocusedWindow, browser: bool, deadline: Instant) -> String {
    let web_area = if browser {
        find_web_area(&focus.window, focus.pid, deadline, 0)
    } else {
        None
    };
    let structure_attrs =
        CFArray::from_CFTypes(&["AXRole", "AXSubrole", "AXChildren"].map(CFString::new));
    let message_attrs =
        CFArray::from_CFTypes(&["AXIdentifier", "AXValue", "AXTitle"].map(CFString::new));
    let mut collector = TextCollector {
        focus,
        structure_attrs,
        message_attrs,
        deadline,
        remaining: WINDOW_CONTEXT_PROMPT_CHARS,
        text: String::new(),
        previous_is_text: false,
    };
    collector.visit(web_area.as_ref().unwrap_or(&focus.window), 0);
    collector.text
}

struct TextCollector<'a> {
    focus: &'a FocusedWindow,
    structure_attrs: CFArray<CFString>,
    message_attrs: CFArray<CFString>,
    deadline: Instant,
    remaining: usize,
    text: String,
    previous_is_text: bool,
}

impl TextCollector<'_> {
    unsafe fn visit(&mut self, element: &SendableElement, depth: usize) {
        if self.finished() || depth >= MAX_DEPTH || !owned_by(element, self.focus.pid) {
            return;
        }
        AXUIElementSetMessagingTimeout(element.as_ref(), MESSAGE_TIMEOUT);
        let Some(attrs) = copy_attributes(element, self.structure_attrs.as_concrete_TypeRef())
        else {
            return;
        };
        let role = attrs.get(0).and_then(|value| string_prefix(*value, 128));
        let subrole = attrs.get(1).and_then(|value| string_prefix(*value, 128));
        let document_editor = subrole.as_deref() == Some("AXApplicationGroup");
        if crate::host_document::evaluate_gate(&GateInputs {
            role: role.clone(),
            subrole,
            ..GateInputs::default()
        })
        .is_some()
        {
            self.previous_is_text = false;
            return;
        }
        let role = role.as_deref().unwrap_or_default();
        let is_input = matches!(
            role,
            "AXTextField" | "AXComboBox" | "AXSearchField" | "AXTextArea"
        );
        let focused_input = is_input && same_element(element, &self.focus.focused);
        if focused_input && !document_editor {
            self.previous_is_text = false;
            return;
        }

        // As in Typeless, nodes without text still delimit consecutive static text.
        let text = match role {
            "AXTextField" | "AXTextArea" | "AXSearchField" | "AXComboBox" | "AXStaticText"
                if !focused_input =>
            {
                copy_attr(element.as_ref(), b"AXValue\0").and_then(|value| {
                    let text = string_prefix(value, self.remaining);
                    CFRelease(value);
                    text
                })
            }
            "AXUnknown" => self.unknown_message_text(element),
            _ => None,
        };
        self.push(text.as_deref().unwrap_or_default(), role == "AXStaticText");
        if self.finished() || depth + 1 == MAX_DEPTH {
            return;
        }
        if let Some(children) = attrs.get(2).and_then(|value| {
            let value = *value;
            (!value.is_null() && CFGetTypeID(value) == CFArray::<*const c_void>::type_id())
                .then(|| CFArray::<*const c_void>::wrap_under_get_rule(value as CFArrayRef))
        }) {
            for child in children.iter() {
                if self.finished() {
                    break;
                }
                if let Some(child) = retained_element(*child) {
                    self.visit(&child, depth + 1);
                }
            }
        }
    }

    unsafe fn unknown_message_text(&self, element: &SendableElement) -> Option<String> {
        let attrs = copy_attributes(element, self.message_attrs.as_concrete_TypeRef())?;
        let identifier = string_prefix(*attrs.get(0)?, 128)?;
        if !matches!(
            identifier.as_str(),
            "MMTextMessageCellView" | "MMTimeStampCellView"
        ) {
            return None;
        }
        [1, 2].into_iter().find_map(|index| {
            string_prefix(*attrs.get(index)?, self.remaining).filter(|text| !text.trim().is_empty())
        })
    }

    fn finished(&self) -> bool {
        self.remaining == 0 || Instant::now() >= self.deadline
    }

    fn push(&mut self, fragment: &str, is_text: bool) {
        let has_text = !fragment.trim().is_empty();
        if has_text {
            self.remaining -= fragment.chars().count();
            if !self.text.is_empty() && !(is_text && self.previous_is_text) {
                self.text.push('\n');
            }
            self.text.push_str(fragment);
        }
        self.previous_is_text = has_text && is_text;
    }
}

unsafe fn find_web_area(
    element: &SendableElement,
    pid: i32,
    deadline: Instant,
    depth: usize,
) -> Option<SendableElement> {
    if depth == MAX_WEB_AREA_DEPTH || Instant::now() >= deadline {
        return None;
    }
    for child in children(element)?.iter() {
        if Instant::now() >= deadline {
            break;
        }
        let Some(child) = retained_element(*child).filter(|child| owned_by(child, pid)) else {
            continue;
        };
        AXUIElementSetMessagingTimeout(child.as_ref(), MESSAGE_TIMEOUT);
        if copy_string_attr(child.as_ref(), b"AXRole\0").as_deref() == Some("AXWebArea") {
            return Some(child);
        }
        if let Some(web_area) = find_web_area(&child, pid, deadline, depth + 1) {
            return Some(web_area);
        }
    }
    None
}

unsafe fn copy_attributes(
    element: &SendableElement,
    attrs: CFArrayRef,
) -> Option<CFArray<*const c_void>> {
    let mut values = std::ptr::null();
    let status = AXUIElementCopyMultipleAttributeValues(element.as_ref(), attrs, 0, &mut values);
    (status == AX_ERROR_SUCCESS && !values.is_null())
        .then(|| CFArray::wrap_under_create_rule(values))
}

unsafe fn string_prefix(value: *const c_void, limit: usize) -> Option<String> {
    if value.is_null() || CFGetTypeID(value) != CFStringGetTypeID() {
        return None;
    }
    let value = value as CFStringRef;
    let length = CFStringGetLength(value);
    let text = if length <= (limit * 2) as isize {
        cfstring_to_rust(value)?
    } else {
        // Two UTF-16 units per scalar is enough for any prefix of the requested length.
        let mut prefix = vec![0; limit * 2];
        CFStringGetCharacters(
            value.cast(),
            CFRange {
                location: 0,
                length: prefix.len() as isize,
            },
            prefix.as_mut_ptr(),
        );
        String::from_utf16_lossy(&prefix)
    };
    Some(text.chars().take(limit).collect())
}

unsafe fn owned_element_attr(
    element: &SendableElement,
    attribute: &[u8],
) -> Option<SendableElement> {
    let value = copy_element_attr(element.as_ref(), attribute)?;
    if CFGetTypeID(value.cast()) == AXUIElementGetTypeID() {
        AXUIElementSetMessagingTimeout(value, MESSAGE_TIMEOUT);
        Some(SendableElement(value as usize))
    } else {
        CFRelease(value.cast());
        None
    }
}

unsafe fn owned_by(element: &SendableElement, pid: i32) -> bool {
    let mut owner = 0;
    AXUIElementGetPid(element.as_ref(), &mut owner) == AX_ERROR_SUCCESS && owner == pid
}

unsafe fn same_element(a: &SendableElement, b: &SendableElement) -> bool {
    CFEqual(a.as_ref().cast(), b.as_ref().cast()) != 0
}

unsafe fn children(element: &SendableElement) -> Option<CFArray<*const c_void>> {
    let value = copy_attr(element.as_ref(), b"AXChildren\0")?;
    if CFGetTypeID(value) != CFArray::<*const c_void>::type_id() {
        CFRelease(value);
        return None;
    }
    Some(CFArray::wrap_under_create_rule(value as CFArrayRef))
}

unsafe fn retained_element(value: *const c_void) -> Option<SendableElement> {
    (!value.is_null() && CFGetTypeID(value) == AXUIElementGetTypeID())
        .then(|| SendableElement::retained(value as AxUiElementRef))
}

unsafe fn bounded_text(element: &SendableElement, attr: &[u8], limit: usize) -> Option<String> {
    let value = copy_attr(element.as_ref(), attr)?;
    let text = (CFGetTypeID(value) == CFStringGetTypeID()
        && CFStringGetLength(value as CFStringRef) <= (limit * 2) as isize)
        .then(|| cfstring_to_rust(value as CFStringRef))
        .flatten()
        .map(|text| text.trim().chars().take(limit).collect::<String>())
        .filter(|text| !text.is_empty());
    CFRelease(value);
    text
}

unsafe fn page_domain(focused: &SendableElement, pid: i32, deadline: Instant) -> Option<String> {
    let mut element = focused.clone();
    for _ in 0..8 {
        if !owned_by(&element, pid)
            || deadline.saturating_duration_since(Instant::now()) < Duration::from_millis(220)
        {
            break;
        }
        if copy_string_attr(element.as_ref(), b"AXRole\0").as_deref() == Some("AXWebArea") {
            let value = copy_attr(element.as_ref(), b"AXURL\0")?;
            let url = if CFGetTypeID(value) == CFURL::type_id() {
                Some(
                    CFURL::wrap_under_get_rule(value as CFURLRef)
                        .get_string()
                        .to_string(),
                )
            } else if CFGetTypeID(value) == CFStringGetTypeID() {
                cfstring_to_rust(value as CFStringRef)
            } else {
                None
            };
            CFRelease(value);
            let url = url::Url::parse(&url?).ok()?;
            return matches!(url.scheme(), "http" | "https")
                .then(|| url.domain().map(str::to_owned))
                .flatten();
        }
        element = owned_element_attr(&element, b"AXParent\0")?;
    }
    None
}

unsafe fn frame(element: &SendableElement) -> Option<CGRect> {
    unsafe fn geometry<T: Default>(element: &SendableElement, attr: &[u8], kind: i32) -> Option<T> {
        let value = copy_attr(element.as_ref(), attr)?;
        if CFGetTypeID(value) != AXValueGetTypeID() {
            CFRelease(value);
            return None;
        }
        let mut output = T::default();
        let ok = AXValueGetValue(value, kind, &mut output as *mut T as *mut c_void);
        CFRelease(value);
        (ok != 0).then_some(output)
    }
    let origin: CGPoint = geometry(element, b"AXPosition\0", AX_VALUE_CGPOINT)?;
    let size: CGSize = geometry(element, b"AXSize\0", AX_VALUE_CGSIZE)?;
    let rect = CGRect::new(&origin, &size);
    (!rect.is_empty()).then_some(rect)
}
