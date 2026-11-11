#import <CoreGraphics/CoreGraphics.h>
#import <ImageIO/ImageIO.h>
#import <ScreenCaptureKit/ScreenCaptureKit.h>

typedef void (*OpenLessScreenshotCallback)(void *context, const uint8_t *bytes, size_t length);

static BOOL matchesBounds(CGRect candidate, CGRect expected) {
    return fabs(candidate.origin.x - expected.origin.x) <= 2 &&
        fabs(candidate.origin.y - expected.origin.y) <= 2 &&
        fabs(candidate.size.width - expected.size.width) <= 2 &&
        fabs(candidate.size.height - expected.size.height) <= 2;
}

static NSData *encodePNG(CGImageRef image) {
    NSMutableData *data = [NSMutableData data];
    CGImageDestinationRef destination =
        CGImageDestinationCreateWithData((__bridge CFMutableDataRef)data, CFSTR("public.png"), 1, NULL);
    if (!destination) return nil;
    CGImageDestinationAddImage(destination, image, NULL);
    BOOL encoded = CGImageDestinationFinalize(destination);
    CFRelease(destination);
    return encoded ? data : nil;
}

static NSData *boundedPNG(CGImageRef image, size_t maxBytes) {
    NSData *data = encodePNG(image);
    if (data.length <= maxBytes) return data;

    // Re-encode a smaller image from this same frame. Keep the entire window.
    CGImageSourceRef source = CGImageSourceCreateWithData((__bridge CFDataRef)data, NULL);
    if (!source) return nil;
    size_t edge = MAX(CGImageGetWidth(image), CGImageGetHeight(image));
    NSDictionary *options = @{
        (__bridge NSString *)kCGImageSourceCreateThumbnailFromImageAlways: @YES,
        (__bridge NSString *)kCGImageSourceThumbnailMaxPixelSize: @((size_t)floor(edge * 0.55)),
    };
    CGImageRef smaller = CGImageSourceCreateThumbnailAtIndex(source, 0,
        (__bridge CFDictionaryRef)options);
    CFRelease(source);
    if (!smaller) return nil;
    data = encodePNG(smaller);
    CGImageRelease(smaller);
    return data.length <= maxBytes ? data : nil;
}

void openless_capture_window_png(pid_t pid, CGRect bounds, size_t maxBytes, void *context,
                                 OpenLessScreenshotCallback callback) {
    if (!CGPreflightScreenCaptureAccess() || CGRectIsEmpty(bounds)) {
        callback(context, NULL, 0);
        return;
    }
    [SCShareableContent getShareableContentExcludingDesktopWindows:YES onScreenWindowsOnly:YES
        completionHandler:^(SCShareableContent *content, NSError *error) {
            SCWindow *matched = nil;
            for (SCWindow *window in content.windows) {
                if (window.owningApplication.processID != pid || window.windowLayer != 0 ||
                    !window.isOnScreen || !matchesBounds(window.frame, bounds)) continue;
                if (matched) {
                    callback(context, NULL, 0);
                    return;
                }
                matched = window;
            }
            if (!matched || error) {
                callback(context, NULL, 0);
                return;
            }
            SCContentFilter *filter = [[SCContentFilter alloc] initWithDesktopIndependentWindow:matched];
            CGSize size = matched.frame.size;
            CGFloat scale = MIN(filter.pointPixelScale, 1600.0 / MAX(size.width, size.height));
            scale = MIN(scale, sqrt(1500000.0 / (size.width * size.height)));
            SCStreamConfiguration *configuration = [SCStreamConfiguration new];
            configuration.width = MAX(1, (size_t)floor(size.width * scale));
            configuration.height = MAX(1, (size_t)floor(size.height * scale));
            configuration.showsCursor = NO;
            configuration.ignoreShadowsSingleWindow = YES;
            [SCScreenshotManager captureImageWithFilter:filter configuration:configuration
                completionHandler:^(CGImageRef image, NSError *captureError) {
                    NSData *data = image && !captureError ? boundedPNG(image, maxBytes) : nil;
                    callback(context, data.bytes, data.length);
                }];
        }];
}
