//! Bounded AX snapshot of the visible foreground window outside the input field.

use std::collections::HashSet;
use std::ffi::c_void;
use std::time::{Duration, Instant};

use core_foundation::array::{CFArray, CFArrayRef};
use core_foundation::base::TCFType;
use core_foundation::boolean::CFBoolean;
use core_foundation::url::{CFURLRef, CFURL};
use core_graphics::geometry::{CGPoint, CGRect, CGSize};
use openless_core::host_document::{
    window_text_meets_screenshot_threshold, WindowCapture, WindowCaptureMode, WindowContext,
    WindowScreenshot, WINDOW_CONTEXT_PROMPT_CHARS,
    WINDOW_SCREENSHOT_MAX_BYTES,
};

use super::{
    cfstring_from_static, cfstring_to_rust, copy_attr, copy_element_attr, copy_string_attr,
    focused_element_passing_the_gate_with_timeout, AXUIElementCreateApplication, AXUIElementGetPid,
    AXUIElementSetMessagingTimeout, AXValueGetValue, AxError, AxUiElementRef, CFEqual, CFGetTypeID,
    CFRelease, CFStringGetLength, CFStringGetTypeID, CFStringRef, CFTypeId, GateInputs,
    GatedElement, SendableElement, AX_ERROR_SUCCESS,
};
use crate::host_document::{BlockReason, HostDocumentReadResult, HostDocumentStatus};
use crate::selection::{self, SelectionInsertionTarget};

const CAPTURE_BUDGET: Duration = Duration::from_millis(600);
const MESSAGE_TIMEOUT: f32 = 0.035;
// Desktop web views can nest their document beyond 24 wrapper levels.
const MAX_NODES: usize = 1_536;
const MAX_DEPTH: usize = 48;
const MAX_CHILDREN: isize = 128;
const MAX_TEXT_CHARS: usize = 10_000;
const AX_VALUE_CGPOINT: i32 = 1;
const AX_VALUE_CGSIZE: i32 = 2;
static CAPTURE_SLOT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXUIElementGetTypeID() -> CFTypeId;
    fn AXValueGetTypeID() -> CFTypeId;
    fn AXUIElementGetAttributeValueCount(
        element: AxUiElementRef,
        attribute: CFStringRef,
        count: *mut isize,
    ) -> AxError;
    fn AXUIElementCopyAttributeValues(
        element: AxUiElementRef,
        attribute: CFStringRef,
        index: isize,
        count: isize,
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
        return capture_for_target(target, WINDOW_CONTEXT_PROMPT_CHARS)
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
            match unsafe { capture(&start_target, &focus, deadline, WINDOW_CONTEXT_PROMPT_CHARS) } {
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
    let mut result = match capture_for_target(target, MAX_TEXT_CHARS).await {
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
    text_limit: usize,
) -> Result<WindowContext, CaptureError> {
    let permit = CAPTURE_SLOT
        .try_acquire()
        .map_err(|_| Unavailable("capture_busy"))?;
    let deadline = Instant::now() + CAPTURE_BUDGET;
    let task = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        unsafe {
            let focus = focused_window(&target, deadline)?;
            capture(&target, &focus, deadline, text_limit)
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
    text_limit: usize,
) -> Result<WindowContext, CaptureError> {
    let FocusedWindow { focused, window, bounds, pid, .. } = focus;
    let focus_y = frame(focused).map(|frame| frame.origin.y + frame.size.height / 2.0);
    let domain = page_domain(focused, *pid, deadline);
    let title = bounded_text(window, b"AXTitle\0", 256);
    let visible_text = visible_text(
        window,
        focused,
        *bounds,
        focus_y,
        *pid,
        deadline,
        text_limit,
    );

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

unsafe fn visible_text(
    window: &SendableElement,
    focused: &SendableElement,
    window_rect: CGRect,
    focus_y: Option<f64>,
    pid: i32,
    deadline: Instant,
    text_limit: usize,
) -> String {
    let focus_path = focused_ancestors(window, focused, pid, deadline);
    let mut stack = vec![(window.clone(), window_rect, 0, false)];
    let mut seen = HashSet::new();
    let mut fragments = Vec::new();
    // Reserve time for the final identity checks. Return the text collected so far.
    let walk_deadline = deadline - Duration::from_millis(65);
    for _ in 0..MAX_NODES {
        if Instant::now() >= walk_deadline {
            break;
        }
        let Some((element, clip, depth, in_document_editor)) = stack.pop() else {
            break;
        };
        if !owned_by(&element, pid) {
            continue;
        }
        AXUIElementSetMessagingTimeout(element.as_ref(), MESSAGE_TIMEOUT);
        let Some(role) = copy_string_attr(element.as_ref(), b"AXRole\0") else {
            continue;
        };
        // Document editors such as Notion expose separate paragraphs as text areas.
        // Keep the active input (and its ancestors) out of the window snapshot.
        let is_text_area = role == "AXTextArea";
        let subrole = is_text_area
            .then(|| copy_string_attr(element.as_ref(), b"AXSubrole\0"))
            .flatten();
        let is_document_editor = is_text_area && subrole.as_deref() == Some("AXApplicationGroup");
        let is_text_input = is_text_area && !is_document_editor;
        if is_text_area
            && ((is_text_input
                && (!in_document_editor
                    || focus_path.iter().any(|node| same_element(node, &element))))
                || crate::host_document::evaluate_gate(&GateInputs {
                    role: Some(role.clone()),
                    subrole,
                    ..GateInputs::default()
                })
                .is_some())
        {
            continue;
        }
        if matches!(
            role.as_str(),
            "AXTextField"
                | "AXComboBox"
                | "AXSearchField"
                | "AXSecureTextField"
                | "AXToolbar"
                | "AXMenuBar"
                | "AXMenu"
                | "AXTab"
                | "AXRadioButton"
        ) || (!is_text_area && bool_attr(&element, b"AXEditable\0"))
            || bool_attr(&element, b"AXHidden\0")
        {
            continue;
        }
        if matches!(role.as_str(), "AXGroup" | "AXList" | "AXRow")
            && frame(&element).is_some_and(|bounds| {
                bounds.size.width <= 1.0
                    || bounds.size.height <= 1.0
                    || intersect(clip, bounds).is_none()
            })
        {
            continue;
        }
        if is_text_input || matches!(role.as_str(), "AXStaticText" | "AXHeading") {
            // Web views can clamp offscreen text to a one-pixel box at the scroll boundary.
            let Some(bounds) = frame(&element).filter(|bounds| {
                bounds.size.width > 1.0 && bounds.size.height > 1.0 && contains_rect(clip, *bounds)
            }) else {
                continue;
            };
            let text = bounded_text(&element, b"AXValue\0", MAX_TEXT_CHARS)
                .or_else(|| bounded_text(&element, b"AXTitle\0", MAX_TEXT_CHARS));
            if let Some(text) = text.filter(|text| seen.insert(text.clone())) {
                let y = bounds.origin.y + bounds.size.height / 2.0;
                fragments.push((focus_y.map_or(0.0, |focus| (focus - y).abs()), text));
            }
            if is_text_input || role == "AXStaticText" {
                continue;
            }
        }
        if depth == MAX_DEPTH {
            continue;
        }
        let is_scroll_area = role == "AXScrollArea";
        let child_clip = if is_scroll_area {
            let Some(intersection) = frame(&element).and_then(|bounds| intersect(clip, bounds))
            else {
                continue;
            };
            intersection
        } else {
            clip
        };
        // Some embedded web views expose an empty visible list on ordinary wrapper groups,
        // even when the group contains the entire document. Leaf frames still pass clipping.
        let mut children = children(&element, b"AXVisibleChildren\0")
            .filter(|visible| is_scroll_area || !visible.is_empty())
            .or_else(|| children(&element, b"AXChildren\0"))
            .unwrap_or_default();
        // Visit the pane containing the input before spending the bounded node budget on
        // navigation sidebars. Sorting text after traversal cannot recover an unvisited pane.
        let preferred = focus_path
            .windows(2)
            .find(|edge| same_element(&edge[1], &element));
        if let Some(index) = preferred.and_then(|edge| {
            children
                .iter()
                .position(|child| same_element(child, &edge[0]))
        }) {
            let child = children.remove(index);
            children.insert(0, child);
        }
        stack.extend(children.into_iter().rev().map(|child| {
            (
                child,
                child_clip,
                depth + 1,
                in_document_editor || is_document_editor,
            )
        }));
    }
    prioritize_fragments(fragments, text_limit)
}

unsafe fn focused_ancestors(
    window: &SendableElement,
    focused: &SendableElement,
    pid: i32,
    deadline: Instant,
) -> Vec<SendableElement> {
    let mut path = vec![focused.clone()];
    for _ in 0..MAX_DEPTH {
        let node = path
            .last()
            .expect("focus path starts with the focused element");
        if same_element(node, window) {
            return path;
        }
        if deadline.saturating_duration_since(Instant::now()) < Duration::from_millis(220) {
            break;
        }
        let Some(parent) = owned_element_attr(node, b"AXParent\0") else {
            break;
        };
        if !owned_by(&parent, pid) || path.iter().any(|node| same_element(node, &parent)) {
            break;
        }
        path.push(parent);
    }
    path
}

fn prioritize_fragments(fragments: Vec<(f64, String)>, text_limit: usize) -> String {
    let mut indexed: Vec<_> = fragments.into_iter().enumerate().collect();
    indexed.sort_by(|a, b| a.1 .0.total_cmp(&b.1 .0));
    let mut remaining = text_limit;
    let mut selected = Vec::new();
    for (index, (_, text)) in indexed {
        if remaining == 0 {
            break;
        }
        let text: String = text.chars().take(remaining).collect();
        remaining = remaining.saturating_sub(text.chars().count() + 1);
        selected.push((index, text));
    }
    selected.sort_by_key(|(index, _)| *index);
    selected
        .into_iter()
        .map(|(_, text)| text)
        .collect::<Vec<_>>()
        .join("\n")
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

unsafe fn children(element: &SendableElement, attribute: &[u8]) -> Option<Vec<SendableElement>> {
    let attr = cfstring_from_static(attribute)?;
    let mut count = 0;
    let mut array = std::ptr::null();
    let status = AXUIElementGetAttributeValueCount(element.as_ref(), attr, &mut count);
    let result = if status == AX_ERROR_SUCCESS && count > 0 {
        AXUIElementCopyAttributeValues(
            element.as_ref(),
            attr,
            0,
            count.min(MAX_CHILDREN),
            &mut array,
        )
    } else {
        status
    };
    CFRelease(attr);
    if result != AX_ERROR_SUCCESS {
        return None;
    }
    if count <= 0 {
        return Some(Vec::new());
    }
    if array.is_null() {
        return None;
    }
    let array: CFArray<*const c_void> = CFArray::wrap_under_create_rule(array);
    Some(
        array
            .iter()
            .filter_map(|value| {
                let value = *value;
                (!value.is_null() && CFGetTypeID(value.cast()) == AXUIElementGetTypeID())
                    .then(|| SendableElement::retained(value as AxUiElementRef))
            })
            .collect(),
    )
}

unsafe fn bool_attr(element: &SendableElement, attr: &[u8]) -> bool {
    let Some(value) = copy_attr(element.as_ref(), attr) else {
        return false;
    };
    let result = CFGetTypeID(value) == CFBoolean::type_id()
        && bool::from(CFBoolean::wrap_under_get_rule(value as _));
    CFRelease(value);
    result
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

fn contains_rect(outer: CGRect, inner: CGRect) -> bool {
    outer.contains(&inner.origin)
        && outer.contains(&CGPoint::new(
            inner.origin.x + inner.size.width - 0.01,
            inner.origin.y + inner.size.height - 0.01,
        ))
}

fn intersect(a: CGRect, b: CGRect) -> Option<CGRect> {
    let x = a.origin.x.max(b.origin.x);
    let y = a.origin.y.max(b.origin.y);
    let right = (a.origin.x + a.size.width).min(b.origin.x + b.size.width);
    let bottom = (a.origin.y + a.size.height).min(b.origin.y + b.size.height);
    (right > x && bottom > y)
        .then(|| CGRect::new(&CGPoint::new(x, y), &CGSize::new(right - x, bottom - y)))
}
