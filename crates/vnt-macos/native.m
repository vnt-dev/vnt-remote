#import <AppKit/AppKit.h>
#import <ApplicationServices/ApplicationServices.h>
#import <ScreenCaptureKit/ScreenCaptureKit.h>
#import <VideoToolbox/VideoToolbox.h>
#import <AVFoundation/AVFoundation.h>
#import <AudioToolbox/AudioToolbox.h>
#import <Security/Security.h>
#include <pthread.h>

static NSString *displayName(CGDirectDisplayID display);

static BOOL sessionIsUnlocked(NSDictionary *session) {
    // The SDK constant's value is "kCGSSessionOnConsoleKey" (with two S's).
    // Use the constant so an unlocked console session is not mistaken for logout.
    return [session[(__bridge NSString *)kCGSessionOnConsoleKey] boolValue]
        && ![session[@"CGSSessionScreenIsLocked"] boolValue];
}
@interface VNTPanel : NSPanel
@end
@implementation VNTPanel
- (BOOL)canBecomeKeyWindow { return YES; }
@end
@interface VNTSelection : NSView
@property NSPoint start;
@property NSPoint end;
@property BOOL dragging;
@property(copy) void (^finish)(id);
@end
@implementation VNTSelection
- (BOOL)acceptsFirstResponder { return YES; }
- (void)drawRect:(NSRect)dirty {
    [[NSColor colorWithWhite:0 alpha:0.25] setFill]; NSRectFill(self.bounds);
    if (self.dragging) {
        NSRect rect = NSMakeRect(MIN(self.start.x,self.end.x),MIN(self.start.y,self.end.y),fabs(self.end.x-self.start.x),fabs(self.end.y-self.start.y));
        [NSColor.systemBlueColor setStroke]; NSFrameRectWithWidth(rect,2);
    }
}
- (void)keyDown:(NSEvent *)event { if (event.keyCode == 53) self.finish(NSNull.null); }
- (void)mouseDown:(NSEvent *)event { self.start = [self convertPoint:event.locationInWindow fromView:nil]; self.end = self.start; self.dragging = YES; self.needsDisplay = YES; }
- (void)mouseDragged:(NSEvent *)event { self.end = [self convertPoint:event.locationInWindow fromView:nil]; self.needsDisplay = YES; }
- (void)mouseUp:(NSEvent *)event {
    [self mouseDragged:event]; NSScreen *screen = self.window.screen;
    CGDirectDisplayID display = [screen.deviceDescription[@"NSScreenNumber"] unsignedIntValue];
    CGDisplayModeRef mode = CGDisplayCopyDisplayMode(display); if (!mode) { self.finish(NSNull.null); return; }
    uint32_t w = (uint32_t)CGDisplayModeGetPixelWidth(mode), h = (uint32_t)CGDisplayModeGetPixelHeight(mode); CGDisplayModeRelease(mode);
    double sx = w/self.bounds.size.width, sy = h/self.bounds.size.height;
    uint32_t x = (uint32_t)(fmax(0,MIN(self.start.x,self.end.x))*sx) & ~1u;
    uint32_t y = (uint32_t)(fmax(0,self.bounds.size.height-MAX(self.start.y,self.end.y))*sy) & ~1u;
    uint32_t width = MIN(w-x,(uint32_t)(fabs(self.start.x-self.end.x)*sx)) & ~1u;
    uint32_t height = MIN(h-y,(uint32_t)(fabs(self.start.y-self.end.y)*sy)) & ~1u;
    if (width < 64 || height < 64) { self.finish(NSNull.null); return; }
    self.finish(@{@"id":@"pending",@"name":@"屏幕区域",@"monitorName":displayName(display),@"screenWidth":@(w),@"screenHeight":@(h),@"orientation":@(((int)CGDisplayRotation(display)/90)%4),@"x":@(x),@"y":@(y),@"width":@(width),@"height":@(height)});
}
@end

static NSString *displayName(CGDirectDisplayID display) {
    CFUUIDRef uuid = CGDisplayCreateUUIDFromDisplayID(display);
    if (!uuid) return @"";
    NSString *name = CFBridgingRelease(CFUUIDCreateString(NULL, uuid));
    CFRelease(uuid);
    return name;
}
static NSArray *monitors(void) {
    CGDirectDisplayID ids[128]; uint32_t count = 0;
    if (CGGetActiveDisplayList(128, ids, &count) != kCGErrorSuccess) return @[];
    NSMutableArray *result = [NSMutableArray array];
    for (uint32_t i=0; i<count; i++) {
        CGDisplayModeRef mode = CGDisplayCopyDisplayMode(ids[i]);
        if (!mode) continue;
        CGRect bounds = CGDisplayBounds(ids[i]);
        [result addObject:@{@"index": @(ids[i]), @"name": displayName(ids[i]),
            @"width": @(CGDisplayModeGetPixelWidth(mode)), @"height": @(CGDisplayModeGetPixelHeight(mode)),
            @"hmonitor": @(ids[i]), @"primary": @((BOOL)(ids[i] == CGMainDisplayID())),
            @"orientation": @(((int)CGDisplayRotation(ids[i]) / 90) % 4),
            @"x": @(bounds.origin.x), @"y": @(bounds.origin.y),
            @"logicalWidth": @(bounds.size.width), @"logicalHeight": @(bounds.size.height)}];
        CGDisplayModeRelease(mode);
    }
    return result;
}
static NSArray *devices(void) {
    AudioObjectPropertyAddress p = {kAudioHardwarePropertyDevices, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
    UInt32 size = 0;
    if (AudioObjectGetPropertyDataSize(kAudioObjectSystemObject, &p, 0, NULL, &size)) return @[];
    NSMutableData *data = [NSMutableData dataWithLength:size];
    if (AudioObjectGetPropertyData(kAudioObjectSystemObject, &p, 0, NULL, &size, data.mutableBytes)) return @[];
    AudioDeviceID defaultID = 0; UInt32 n = sizeof(defaultID);
    AudioObjectPropertyAddress d = {kAudioHardwarePropertyDefaultOutputDevice, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
    AudioObjectGetPropertyData(kAudioObjectSystemObject, &d, 0, NULL, &n, &defaultID);
    NSMutableArray *result = [NSMutableArray array];
    for (UInt32 i=0; i<size/sizeof(AudioDeviceID); i++) {
        AudioDeviceID device = ((AudioDeviceID *)data.bytes)[i];
        AudioObjectPropertyAddress s = {kAudioDevicePropertyStreams, kAudioDevicePropertyScopeOutput, kAudioObjectPropertyElementMain};
        UInt32 streams = 0;
        if (AudioObjectGetPropertyDataSize(device, &s, 0, NULL, &streams) || streams == 0) continue;
        CFStringRef name = NULL, uid = NULL; UInt32 bytes = sizeof(CFStringRef);
        s.mScope = kAudioObjectPropertyScopeGlobal; s.mSelector = kAudioObjectPropertyName;
        AudioObjectGetPropertyData(device, &s, 0, NULL, &bytes, &name);
        s.mSelector = kAudioDevicePropertyDeviceUID; bytes = sizeof(CFStringRef);
        AudioObjectGetPropertyData(device, &s, 0, NULL, &bytes, &uid);
        if (name && uid) [result addObject:@{@"id": (__bridge NSString *)uid, @"name": (__bridge NSString *)name, @"isDefault": @((BOOL)(device == defaultID)), @"nativeId": @(device)}];
        if (name) CFRelease(name); if (uid) CFRelease(uid);
    }
    return result;
}
char *vnt_json(int op, const char *input) {
    @autoreleasepool {
        id arg = [NSJSONSerialization JSONObjectWithData:[[NSString stringWithUTF8String:input] dataUsingEncoding:NSUTF8StringEncoding] options:NSJSONReadingFragmentsAllowed error:nil];
        id result = @{};
        switch (op) {
        case 0: result = monitors(); break;
        case 1: result = @{@"screenRecording": @((BOOL)CGPreflightScreenCaptureAccess()), @"accessibility": @((BOOL)AXIsProcessTrusted())}; break;
        case 2: {
            NSBundle *bundle = NSBundle.mainBundle;
            if (![bundle.bundlePath.pathExtension isEqualToString:@"app"] || ![bundle.bundleIdentifier isEqualToString:@"com.vnt.remote"]) {
                result = @{@"error": @"当前运行的是未打包的开发程序，系统可能无法将权限关联到 vnt-remote。请运行 npm run dev:macos，从生成的 vnt-remote.app 中请求授权。"};
                break;
            }
            if ([arg isEqual:@"screenRecording"]) {
                if (CGPreflightScreenCaptureAccess()) break;
                // Use the same API as capture to trigger TCC. Merely requesting the
                // legacy CoreGraphics permission can return false without a prompt.
                // This operation is called from a blocking worker, never the UI thread.
                dispatch_semaphore_t ready = dispatch_semaphore_create(0);
                __block BOOL granted = NO;
                [SCShareableContent getShareableContentExcludingDesktopWindows:YES onScreenWindowsOnly:YES completionHandler:^(SCShareableContent *content, NSError *error) {
                    granted = content != nil && error == nil;
                    dispatch_semaphore_signal(ready);
                }];
                if (dispatch_semaphore_wait(ready, dispatch_time(DISPATCH_TIME_NOW, 60*NSEC_PER_SEC))) {
                    result = @{@"error": @"等待屏幕录制授权超时，请检查系统授权弹窗或系统设置。授权后请退出并重新打开应用。"};
                } else if (!granted || !CGPreflightScreenCaptureAccess()) {
                    result = @{@"error": @"屏幕录制权限尚未生效。请在系统设置 → 隐私与安全性 → 屏幕与系统音频录制中开启 vnt-remote；若已开启，请退出并重新打开应用。列表中没有应用时，可点击「+」添加当前 vnt-remote.app。"};
                }
            } else if ([arg isEqual:@"accessibility"]) {
                __block BOOL trusted = NO;
                void (^request)(void) = ^{ trusted = AXIsProcessTrustedWithOptions((__bridge CFDictionaryRef)@{(__bridge NSString *)kAXTrustedCheckOptionPrompt: @YES}); };
                if (NSThread.isMainThread) request(); else dispatch_sync(dispatch_get_main_queue(), request);
                if (!trusted) result = @{@"error": @"请在系统设置 → 隐私与安全性 → 辅助功能中开启 vnt-remote，然后返回应用。"};
            }
            break;
        }
        case 3: {
            NSString *url = [arg isKindOfClass:NSString.class] ? arg : @"";
            if (!([url hasPrefix:@"https://github.com/vnt-dev/vnt-remote/"] || [url hasPrefix:@"x-apple.systempreferences:com.apple.preference.security?Privacy_"])) { result = @{@"error": @"不允许的外部地址"}; break; }
            __block BOOL opened = NO;
            void (^open)(void) = ^{ opened = [NSWorkspace.sharedWorkspace openURL:[NSURL URLWithString:url]]; };
            if (NSThread.isMainThread) open(); else dispatch_sync(dispatch_get_main_queue(), open);
            if (!opened) result = @{@"error": @"无法打开系统设置或外部地址，请手动打开。"};
            break;
        }
        case 4: result = @([NSPasteboard generalPasteboard].changeCount); break;
        case 5: {
            NSPasteboard *p = NSPasteboard.generalPasteboard;
            if ([p.types containsObject:@"com.vnt.remote.origin"]) { result = @{@"kind": @"ours"}; break; }
            if ([p.types containsObject:NSPasteboardTypeFileURL]) { result = @{@"kind": @"other"}; break; }
            NSData *image = [p dataForType:NSPasteboardTypePNG];
            if (!image && [p.types containsObject:NSPasteboardTypeTIFF]) {
                NSData *tiff = [p dataForType:NSPasteboardTypeTIFF];
                if (tiff.length <= 64*1024*1024) {
                    NSBitmapImageRep *rep = [NSBitmapImageRep imageRepWithData:tiff];
                    if (rep.pixelsWide <= 16384 && rep.pixelsHigh <= 16384 && (uint64_t)rep.pixelsWide*rep.pixelsHigh*4 <= 64*1024*1024)
                        image = [rep representationUsingType:NSBitmapImageFileTypePNG properties:@{}];
                }
            }
            if (image.length > 32*1024*1024) { result = @{@"error": @"图片超过剪贴板上限"}; break; }
            NSString *text = [p stringForType:NSPasteboardTypeString];
            if (image) result = @{@"kind": @"image", @"data": [image base64EncodedStringWithOptions:0]};
            else if (text && [text lengthOfBytesUsingEncoding:NSUTF8StringEncoding] <= 4*1024*1024) result = @{@"kind": @"text", @"text": text};
            else result = @{@"kind": @"other"};
            break;
        }
        case 6: {
            NSPasteboard *p = NSPasteboard.generalPasteboard;
            if (p.changeCount != [arg[@"sequence"] unsignedIntValue]) { result = @{@"error": @"剪贴板已变化"}; break; }
            NSPasteboardItem *item = [NSPasteboardItem new];
            BOOL ok;
            if (arg[@"text"]) ok = [item setString:arg[@"text"] forType:NSPasteboardTypeString];
            else { NSData *data = [[NSData alloc] initWithBase64EncodedString:arg[@"image"] options:0]; ok = data && [item setData:data forType:NSPasteboardTypePNG]; }
            [item setString:@"vnt" forType:@"com.vnt.remote.origin"];
            if (ok) { [p clearContents]; ok = [p writeObjects:@[item]]; }
            if (!ok) result = @{@"error": @"写入系统剪贴板失败"};
            break;
        }
        case 7: result = devices(); break;
        case 8: { // Keychain: config contains an opaque account reference, never a password.
            NSMutableDictionary *q = [@{(__bridge id)kSecClass: (__bridge id)kSecClassGenericPassword,
                (__bridge id)kSecAttrService: @"com.vnt.remote.host", (__bridge id)kSecAttrAccount: arg[@"account"]} mutableCopy];
            OSStatus status;
            if ([arg[@"delete"] boolValue]) {
                status = SecItemDelete((__bridge CFDictionaryRef)q);
            } else if (arg[@"password"]) {
                NSData *data = [arg[@"password"] dataUsingEncoding:NSUTF8StringEncoding];
                status = SecItemUpdate((__bridge CFDictionaryRef)q, (__bridge CFDictionaryRef)@{(__bridge id)kSecValueData:data});
                if (status == errSecItemNotFound) { q[(__bridge id)kSecValueData] = data; q[(__bridge id)kSecAttrAccessible] = (__bridge id)kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly; status = SecItemAdd((__bridge CFDictionaryRef)q, NULL); }
            } else {
                q[(__bridge id)kSecReturnData] = @YES; CFTypeRef value = NULL;
                status = SecItemCopyMatching((__bridge CFDictionaryRef)q, &value);
                if (status == errSecSuccess) { NSData *data = CFBridgingRelease(value); result = @{ @"password": [[NSString alloc] initWithData:data encoding:NSUTF8StringEncoding] ?: @"" }; }
            }
            if (status != errSecSuccess) result = @{@"error": [NSString stringWithFormat:@"Keychain 访问失败：%d", (int)status]};
            break;
        }
        case 9: {
            CFDictionaryRef session = CGSessionCopyCurrentDictionary();
            NSDictionary *s = CFBridgingRelease(session);
            result = @((BOOL)(sessionIsUnlocked(s) && CGPreflightScreenCaptureAccess()));
            break;
        }
        case 10: { CGEventRef e = CGEventCreate(NULL); CGPoint p = CGEventGetLocation(e); CFRelease(e); result = @{@"x": @(p.x), @"y": @(p.y)}; break; }
        case 11: {
            if (!CGPreflightScreenCaptureAccess()) { result = @{@"error":@"请先授予屏幕录制权限"}; break; }
            __block id selection = NSNull.null;
            dispatch_sync(dispatch_get_main_queue(), ^{
                NSMutableArray<NSWindow *> *panels = [NSMutableArray array];
                for (NSScreen *screen in NSScreen.screens) {
                    VNTPanel *panel = [[VNTPanel alloc] initWithContentRect:screen.frame styleMask:NSWindowStyleMaskBorderless backing:NSBackingStoreBuffered defer:NO];
                    panel.worksWhenModal = YES; panel.level = NSScreenSaverWindowLevel; panel.opaque = NO; panel.backgroundColor = NSColor.clearColor; panel.releasedWhenClosed = NO;
                    VNTSelection *view = [[VNTSelection alloc] initWithFrame:NSMakeRect(0,0,screen.frame.size.width,screen.frame.size.height)];
                    view.finish = ^(id value) { selection = value; [NSApp stopModal]; };
                    panel.contentView = view; [panel makeFirstResponder:view]; [panels addObject:panel]; [panel orderFrontRegardless];
                }
                id escape = [NSEvent addLocalMonitorForEventsMatchingMask:NSEventMaskKeyDown handler:^NSEvent *(NSEvent *event) {
                    if (event.keyCode == 53) { [NSApp stopModal]; return nil; } return event;
                }];
                if (panels.count) { [panels[0] makeKeyAndOrderFront:nil]; [NSApp runModalForWindow:panels[0]]; }
                [NSEvent removeMonitor:escape];
                for (NSWindow *panel in panels) { ((VNTSelection *)panel.contentView).finish = nil; [panel close]; }
            });
            result = selection; break;
        }
        default: result = @{@"error": @"未知 macOS 操作"}; break;
        }
        NSData *data = [NSJSONSerialization dataWithJSONObject:result options:NSJSONWritingFragmentsAllowed error:nil];
        return data ? strndup(data.bytes, data.length) : strdup("{\"error\":\"原生序列化失败\"}");
    }
}
void vnt_free(void *p) { free(p); }

bool vnt_clipboard_image(const uint8_t *bytes, size_t length, uint32_t sequence, const void *context, bool (*valid)(const void *)) {
    @autoreleasepool {
        if (length > 32*1024*1024) return false;
        NSData *data = [NSData dataWithBytes:bytes length:length];
        NSPasteboardItem *item = [NSPasteboardItem new];
        if (![item setData:data forType:NSPasteboardTypePNG]) return false;
        [item setString:@"vnt" forType:@"com.vnt.remote.origin"];
        NSPasteboard *p = NSPasteboard.generalPasteboard;
        // Check after preparing native data so cancellation cannot be delayed by
        // base64/JSON conversion of a large image before publishing.
        if (p.changeCount != sequence || !valid(context)) return false;
        [p clearContents]; return [p writeObjects:@[item]];
    }
}

@interface VNTStream : NSObject <SCStreamOutput, SCStreamDelegate>
@property SCStream *stream;
@property NSCondition *condition;
@property NSMutableData *pixels;
@property NSMutableData *audio;
@property uint32_t width;
@property uint32_t height;
@property BOOL failed;
@property BOOL audioOnly;
@end
@implementation VNTStream
- (void)stream:(SCStream *)stream didStopWithError:(NSError *)error {
    [self.condition lock]; self.failed = YES; [self.condition broadcast]; [self.condition unlock];
}
- (void)stream:(SCStream *)stream didOutputSampleBuffer:(CMSampleBufferRef)sample ofType:(SCStreamOutputType)type {
    if (!CMSampleBufferIsValid(sample)) return;
    if (type == SCStreamOutputTypeScreen) {
        CVPixelBufferRef pixel = CMSampleBufferGetImageBuffer(sample); if (!pixel) return;
        CFArrayRef attachments = CMSampleBufferGetSampleAttachmentsArray(sample, false);
        if (attachments && CFArrayGetCount(attachments)) {
            NSDictionary *a = (__bridge NSDictionary *)CFArrayGetValueAtIndex(attachments, 0);
            if ([a[SCStreamFrameInfoStatus] integerValue] != SCFrameStatusComplete) return;
        }
        CVPixelBufferLockBaseAddress(pixel, kCVPixelBufferLock_ReadOnly);
        size_t w = CVPixelBufferGetWidth(pixel), h = CVPixelBufferGetHeight(pixel), stride = CVPixelBufferGetBytesPerRow(pixel);
        if (w && h && w <= 16384 && h <= 16384 && w*h <= 64*1024*1024) {
            [self.condition lock]; self.width = (uint32_t)w; self.height = (uint32_t)h;
            [self.pixels setLength:w*h*4];
            for (size_t row=0; row<h; row++) memcpy((uint8_t *)self.pixels.mutableBytes+row*w*4, (uint8_t *)CVPixelBufferGetBaseAddress(pixel)+row*stride, w*4);
            [self.condition signal]; [self.condition unlock];
        }
        CVPixelBufferUnlockBaseAddress(pixel, kCVPixelBufferLock_ReadOnly);
    } else if (type == SCStreamOutputTypeAudio) {
        const AudioStreamBasicDescription *format = CMAudioFormatDescriptionGetStreamBasicDescription(CMSampleBufferGetFormatDescription(sample));
        if (!format || format->mFormatID != kAudioFormatLinearPCM || !(format->mFormatFlags & kAudioFormatFlagIsFloat) || format->mBitsPerChannel != 32) return;
        size_t required = 0; CMBlockBufferRef retained = NULL;
        CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(sample, &required, NULL, 0, NULL, NULL, 0, NULL);
        AudioBufferList *list = malloc(required);
        if (CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(sample, NULL, list, required, NULL, NULL, 0, &retained) == noErr) {
            size_t frames = CMSampleBufferGetNumSamples(sample);
            NSMutableData *pcm = [NSMutableData dataWithLength:frames*2*sizeof(float)]; float *dest = pcm.mutableBytes;
            for (size_t i=0; i<frames; i++) for (size_t c=0; c<2; c++) {
                size_t channel = MIN(c, format->mChannelsPerFrame-1);
                if (format->mFormatFlags & kAudioFormatFlagIsNonInterleaved) dest[i*2+c] = ((float *)list->mBuffers[MIN(channel, list->mNumberBuffers-1)].mData)[i];
                else dest[i*2+c] = ((float *)list->mBuffers[0].mData)[i*format->mChannelsPerFrame+channel];
            }
            [self.condition lock];
            if (self.audio.length + pcm.length > 48000*2*sizeof(float)/5) [self.audio setLength:0];
            [self.audio appendData:pcm]; [self.condition signal]; [self.condition unlock];
        }
        if (retained) CFRelease(retained); free(list);
    }
}
@end
void *vnt_stream_open(uint32_t display, bool audio) {
    @autoreleasepool {
        if (!CGPreflightScreenCaptureAccess()) return NULL;
        dispatch_semaphore_t ready = dispatch_semaphore_create(0);
        __block SCShareableContent *content = nil;
        [SCShareableContent getShareableContentExcludingDesktopWindows:NO onScreenWindowsOnly:YES completionHandler:^(SCShareableContent *value, NSError *error) { content = value; dispatch_semaphore_signal(ready); }];
        if (dispatch_semaphore_wait(ready, dispatch_time(DISPATCH_TIME_NOW, 5*NSEC_PER_SEC))) return NULL;
        SCDisplay *selected = nil;
        for (SCDisplay *d in content.displays) if (d.displayID == (display ?: CGMainDisplayID())) selected = d;
        if (!selected) return NULL;
        VNTStream *s = [VNTStream new]; s.condition = [NSCondition new]; s.pixels = [NSMutableData data]; s.audio = [NSMutableData data]; s.audioOnly = audio;
        SCContentFilter *filter = [[SCContentFilter alloc] initWithDisplay:selected excludingWindows:@[]];
        SCStreamConfiguration *config = [SCStreamConfiguration new];
        CGDisplayModeRef mode = CGDisplayCopyDisplayMode(selected.displayID);
        if (!mode) return NULL;
        config.width = CGDisplayModeGetPixelWidth(mode); config.height = CGDisplayModeGetPixelHeight(mode); CGDisplayModeRelease(mode);
        config.pixelFormat = kCVPixelFormatType_32BGRA; config.showsCursor = YES;
        config.minimumFrameInterval = CMTimeMake(1, 60); config.queueDepth = 3;
        config.capturesAudio = audio; config.sampleRate = 48000; config.channelCount = 2; config.excludesCurrentProcessAudio = YES;
        s.stream = [[SCStream alloc] initWithFilter:filter configuration:config delegate:s];
        NSError *error = nil;
        if (![s.stream addStreamOutput:s type:audio ? SCStreamOutputTypeAudio : SCStreamOutputTypeScreen sampleHandlerQueue:dispatch_queue_create("com.vnt.capture", DISPATCH_QUEUE_SERIAL) error:&error]) return NULL;
        __block BOOL started = NO; dispatch_semaphore_t start = dispatch_semaphore_create(0);
        [s.stream startCaptureWithCompletionHandler:^(NSError *e) { started = !e; dispatch_semaphore_signal(start); }];
        if (dispatch_semaphore_wait(start, dispatch_time(DISPATCH_TIME_NOW, 5*NSEC_PER_SEC)) || !started) {
            [s.stream removeStreamOutput:s type:audio ? SCStreamOutputTypeAudio : SCStreamOutputTypeScreen error:nil];
            [s.stream stopCaptureWithCompletionHandler:^(NSError *e) {}]; s.stream = nil; return NULL;
        }
        return (__bridge_retained void *)s;
    }
}
void vnt_stream_close(void *p) {
    @autoreleasepool {
        VNTStream *s = CFBridgingRelease(p); SCStream *stream = s.stream; s.stream = nil;
        [stream removeStreamOutput:s type:s.audioOnly ? SCStreamOutputTypeAudio : SCStreamOutputTypeScreen error:nil];
        [stream stopCaptureWithCompletionHandler:^(NSError *e) {}];
    }
}
intptr_t vnt_stream_read(void *p, uint8_t *output, size_t capacity, uint32_t *width, uint32_t *height) {
    @autoreleasepool {
        VNTStream *s = (__bridge VNTStream *)p; [s.condition lock];
        if (!(s.audioOnly ? s.audio.length : s.pixels.length) && !s.failed) [s.condition waitUntilDate:[NSDate dateWithTimeIntervalSinceNow:0.05]];
        NSData *data = s.audioOnly ? s.audio : s.pixels;
        *width = s.width; *height = s.height;
        intptr_t n = s.failed ? -1 : data.length > capacity ? -2 : (intptr_t)data.length;
        if (n > 0) memcpy(output, data.bytes, n);
        if (s.audioOnly && n >= 0) [s.audio setLength:0];
        [s.condition unlock]; return n;
    }
}
bool vnt_input(uint32_t display, int kind, double a, double b, bool down) {
    if (!AXIsProcessTrusted() || !CGDisplayIsActive(display)) return false;
    CGEventRef current = CGEventCreate(NULL); CGPoint point = CGEventGetLocation(current); CFRelease(current);
    static pthread_mutex_t inputLock = PTHREAD_MUTEX_INITIALIZER;
    static bool keys[128] = {0}, buttons[5] = {0};
    static CFAbsoluteTime clickTime = 0;
    static CGPoint clickPoint;
    static int clickButton = -1, clickCount = 0;
    pthread_mutex_lock(&inputLock);
    CGEventRef event = NULL;
    if (kind == 0) {
        CGRect r = CGDisplayBounds(display); point = CGPointMake(r.origin.x+fmax(0,fmin(1,a))*fmax(0,r.size.width-1), r.origin.y+fmax(0,fmin(1,b))*fmax(0,r.size.height-1));
        CGEventType type = (buttons[0] || CGEventSourceButtonState(kCGEventSourceStateCombinedSessionState, kCGMouseButtonLeft)) ? kCGEventLeftMouseDragged : (buttons[1] || CGEventSourceButtonState(kCGEventSourceStateCombinedSessionState, kCGMouseButtonRight)) ? kCGEventRightMouseDragged : kCGEventMouseMoved;
        event = CGEventCreateMouseEvent(NULL, type, point, kCGMouseButtonLeft);
    } else if (kind == 1) {
        int button = (int)a; CGEventType type = button == 0 ? (down ? kCGEventLeftMouseDown : kCGEventLeftMouseUp) : button == 1 ? (down ? kCGEventRightMouseDown : kCGEventRightMouseUp) : (down ? kCGEventOtherMouseDown : kCGEventOtherMouseUp);
        event = CGEventCreateMouseEvent(NULL, type, point, button);
        if (button >= 0 && button < 5) buttons[button] = down;
        if (down) {
            CFAbsoluteTime now = CFAbsoluteTimeGetCurrent();
            if (button == clickButton && now-clickTime <= NSEvent.doubleClickInterval && hypot(point.x-clickPoint.x,point.y-clickPoint.y) <= 4) clickCount = MIN(3,clickCount+1);
            else clickCount = 1;
            clickTime = now; clickPoint = point; clickButton = button;
        }
        CGEventSetIntegerValueField(event, kCGMouseEventClickState, MAX(1,clickCount));
    } else if (kind == 2) event = CGEventCreateScrollWheelEvent(NULL, kCGScrollEventUnitPixel, 2, (int)(b*40), (int)(a*40));
    else if (kind == 3) event = CGEventCreateKeyboardEvent(NULL, (CGKeyCode)a, down);
    if (!event) { pthread_mutex_unlock(&inputLock); return false; }
    if (kind == 3 && a >= 0 && a < 128) keys[(int)a] = down;
    CGEventFlags modifiers = 0;
    if (keys[54] || keys[55]) modifiers |= kCGEventFlagMaskCommand;
    if (keys[56] || keys[60]) modifiers |= kCGEventFlagMaskShift;
    if (keys[58] || keys[61]) modifiers |= kCGEventFlagMaskAlternate;
    if (keys[59] || keys[62]) modifiers |= kCGEventFlagMaskControl;
    CGEventSetFlags(event, modifiers);
    CGEventSetIntegerValueField(event, kCGEventSourceUserData, 0x564e5444);
    CGEventPost(kCGHIDEventTap, event); CFRelease(event);
    pthread_mutex_unlock(&inputLock); return true;
}

typedef struct { VTCompressionSessionRef session; uint32_t width, height; NSMutableData *packet; BOOL key; OSStatus error; } VNTEncoder;
static void encoded(void *ref, void *source, OSStatus status, VTEncodeInfoFlags flags, CMSampleBufferRef sample) {
    VNTEncoder *e = ref; e->error = status; if (status || !sample || !CMSampleBufferDataIsReady(sample)) return;
    NSArray *attachments = (__bridge NSArray *)CMSampleBufferGetSampleAttachmentsArray(sample, false);
    e->key = !attachments.count || ![attachments[0][(__bridge NSString *)kCMSampleAttachmentKey_NotSync] boolValue];
    CMFormatDescriptionRef format = CMSampleBufferGetFormatDescription(sample);
    BOOL hevc = CMFormatDescriptionGetMediaSubType(format) == kCMVideoCodecType_HEVC;
    int header = 4;
    if (e->key) {
        size_t count = 0;
        if (hevc) CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(format, 0, NULL, NULL, &count, &header);
        else CMVideoFormatDescriptionGetH264ParameterSetAtIndex(format, 0, NULL, NULL, &count, &header);
        for (size_t i=0; i<count; i++) {
            const uint8_t *bytes = NULL; size_t length = 0;
            OSStatus s = hevc ? CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(format, i, &bytes, &length, NULL, NULL) : CMVideoFormatDescriptionGetH264ParameterSetAtIndex(format, i, &bytes, &length, NULL, NULL);
            if (!s) { [e->packet appendBytes:"\0\0\0\1" length:4]; [e->packet appendBytes:bytes length:length]; }
        }
    }
    CMBlockBufferRef block = CMSampleBufferGetDataBuffer(sample); size_t length = CMBlockBufferGetDataLength(block);
    NSMutableData *data = [NSMutableData dataWithLength:length];
    if (CMBlockBufferCopyDataBytes(block, 0, length, data.mutableBytes)) { e->error = -1; return; }
    const uint8_t *bytes = data.bytes; size_t at = 0;
    while (at+(size_t)header <= length) {
        uint32_t n = 0; for (int j=0; j<header; j++) n = (n<<8) | bytes[at++];
        if (n > length-at) { e->error = -1; return; }
        [e->packet appendBytes:"\0\0\0\1" length:4]; [e->packet appendBytes:bytes+at length:n]; at += n;
    }
}
void *vnt_encoder_open(bool hevc, uint32_t w, uint32_t h, uint32_t fps, uint32_t bitrate) {
    @autoreleasepool {
        VNTEncoder *e = calloc(1, sizeof(*e)); e->width = w; e->height = h;
        NSDictionary *spec = @{(__bridge NSString *)kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder: @YES};
        OSStatus s = VTCompressionSessionCreate(NULL, w, h, hevc ? kCMVideoCodecType_HEVC : kCMVideoCodecType_H264, (__bridge CFDictionaryRef)spec, NULL, NULL, encoded, e, &e->session);
        if (s) { free(e); return NULL; }
        s = VTSessionSetProperty(e->session, kVTCompressionPropertyKey_RealTime, kCFBooleanTrue);
        s |= VTSessionSetProperty(e->session, kVTCompressionPropertyKey_AllowFrameReordering, kCFBooleanFalse);
        s |= VTSessionSetProperty(e->session, kVTCompressionPropertyKey_AverageBitRate, (__bridge CFNumberRef)@(bitrate*1000));
        s |= VTSessionSetProperty(e->session, kVTCompressionPropertyKey_ExpectedFrameRate, (__bridge CFNumberRef)@(fps));
        s |= VTSessionSetProperty(e->session, kVTCompressionPropertyKey_MaxKeyFrameInterval, (__bridge CFNumberRef)@(MAX(fps*2,30)));
        s |= VTSessionSetProperty(e->session, kVTCompressionPropertyKey_ProfileLevel, hevc ? kVTProfileLevel_HEVC_Main_AutoLevel : kVTProfileLevel_H264_High_AutoLevel);
        s |= VTCompressionSessionPrepareToEncodeFrames(e->session);
        if (s) { VTCompressionSessionInvalidate(e->session); CFRelease(e->session); free(e); return NULL; }
        return e;
    }
}
void vnt_encoder_close(void *p) { VNTEncoder *e = p; VTCompressionSessionInvalidate(e->session); CFRelease(e->session); e->packet = nil; free(e); }
intptr_t vnt_encoder_frame(void *p, const uint8_t *pixels, uint64_t pts, bool key, uint8_t *output, size_t capacity, bool *isKey) {
    @autoreleasepool {
        VNTEncoder *e = p; e->packet = [NSMutableData data]; e->error = 0;
        CVPixelBufferRef pixel = NULL;
        if (CVPixelBufferCreate(NULL, e->width, e->height, kCVPixelFormatType_32BGRA, NULL, &pixel)) return -1;
        CVPixelBufferLockBaseAddress(pixel, 0); size_t stride = CVPixelBufferGetBytesPerRow(pixel);
        for (size_t y=0; y<e->height; y++) memcpy((uint8_t *)CVPixelBufferGetBaseAddress(pixel)+y*stride, pixels+y*e->width*4, e->width*4);
        CVPixelBufferUnlockBaseAddress(pixel, 0);
        NSDictionary *options = key ? @{(__bridge NSString *)kVTEncodeFrameOptionKey_ForceKeyFrame:@YES} : nil;
        OSStatus s = VTCompressionSessionEncodeFrame(e->session, pixel, CMTimeMake(pts, 1000), kCMTimeInvalid, (__bridge CFDictionaryRef)options, NULL, NULL);
        CVPixelBufferRelease(pixel);
        if (!s) s = VTCompressionSessionCompleteFrames(e->session, kCMTimeInvalid);
        if (s || e->error || e->packet.length > capacity) return -1;
        memcpy(output, e->packet.bytes, e->packet.length); *isKey = e->key; return e->packet.length;
    }
}

@interface VNTRender : NSObject
@property AVAudioEngine *engine;
@property AVAudioSourceNode *source;
@property NSLock *lock;
@property NSMutableData *samples;
@property AudioDeviceID device;
@end
@implementation VNTRender
@end
void *vnt_render_open(const char *uid) {
    @autoreleasepool {
        VNTRender *r = [VNTRender new]; r.engine = [AVAudioEngine new]; r.lock = [NSLock new]; r.samples = [NSMutableData data];
        NSString *selected = [NSString stringWithUTF8String:uid];
        for (NSDictionary *d in devices()) if ([d[@"id"] isEqual:selected]) r.device = [d[@"nativeId"] unsignedIntValue];
        if (!r.device) for (NSDictionary *d in devices()) if ([d[@"isDefault"] boolValue]) r.device = [d[@"nativeId"] unsignedIntValue];
        if (!r.device) return NULL;
        AudioDeviceID nativeDevice = r.device;
        if (AudioUnitSetProperty(r.engine.outputNode.audioUnit, kAudioOutputUnitProperty_CurrentDevice, kAudioUnitScope_Global, 0, &nativeDevice, sizeof(AudioDeviceID))) return NULL;
        __weak VNTRender *weak = r;
        AVAudioFormat *format = [[AVAudioFormat alloc] initStandardFormatWithSampleRate:48000 channels:2];
        r.source = [[AVAudioSourceNode alloc] initWithFormat:format renderBlock:^OSStatus(BOOL *silent, const AudioTimeStamp *time, AVAudioFrameCount count, AudioBufferList *buffers) {
            VNTRender *strong = weak;
            for (UInt32 c=0; c<buffers->mNumberBuffers; c++) memset(buffers->mBuffers[c].mData, 0, buffers->mBuffers[c].mDataByteSize);
            if (!strong || ![strong.lock tryLock]) return noErr;
            size_t frames = MIN(count, strong.samples.length/(2*sizeof(float))); const float *samples = strong.samples.bytes;
            for (size_t i=0; i<frames; i++) for (size_t c=0; c<MIN(2,buffers->mNumberBuffers); c++) ((float *)buffers->mBuffers[c].mData)[i] = samples[i*2+c];
            [strong.samples replaceBytesInRange:NSMakeRange(0, frames*2*sizeof(float)) withBytes:NULL length:0];
            [strong.lock unlock]; return noErr;
        }];
        [r.engine attachNode:r.source]; [r.engine connect:r.source to:r.engine.mainMixerNode format:format];
        NSError *error = nil; if (![r.engine startAndReturnError:&error]) return NULL;
        return (__bridge_retained void *)r;
    }
}
void vnt_render_close(void *p) { @autoreleasepool { VNTRender *r = CFBridgingRelease(p); [r.engine stop]; } }
bool vnt_render_write(void *p, const float *samples, size_t count) {
    @autoreleasepool {
        VNTRender *r = (__bridge VNTRender *)p;
        if (!r.engine.running) return false;
        UInt32 alive = 0, size = sizeof(alive); AudioObjectPropertyAddress property = {kAudioDevicePropertyDeviceIsAlive,kAudioObjectPropertyScopeGlobal,kAudioObjectPropertyElementMain};
        if (AudioObjectGetPropertyData(r.device, &property, 0, NULL, &size, &alive) || !alive) return false;
        [r.lock lock]; if (r.samples.length+count*sizeof(float) > 48000*2*sizeof(float)*0.06) [r.samples setLength:0];
        [r.samples appendBytes:samples length:count*sizeof(float)]; [r.lock unlock]; return true;
    }
}

@interface VNTKeyboard : NSObject
@property NSWindow *window;
@property id monitor;
@property NSLock *lock;
@property NSMutableArray *events;
@property NSRect area;
@property BOOL overflow;
@end
@implementation VNTKeyboard
@end
static BOOL keyboardActive(VNTKeyboard *k) {
    if (!k.window.isKeyWindow || !k.window.isVisible) return NO;
    NSPoint p = [k.window.contentView convertPoint:[k.window convertPointFromScreen:NSEvent.mouseLocation] fromView:nil];
    CGFloat scale = k.window.backingScaleFactor;
    p = NSMakePoint(p.x*scale, (k.window.contentView.bounds.size.height-p.y)*scale);
    return NSPointInRect(p,k.area);
}
void *vnt_keyboard_open(uintptr_t pointer) {
    @autoreleasepool {
        VNTKeyboard *k = [VNTKeyboard new]; k.lock = [NSLock new]; k.events = [NSMutableArray array];
        k.window = (__bridge NSWindow *)(void *)pointer;
        dispatch_async(dispatch_get_main_queue(), ^{
            __weak VNTKeyboard *weak = k;
            k.monitor = [NSEvent addLocalMonitorForEventsMatchingMask:(NSEventMaskKeyDown|NSEventMaskKeyUp|NSEventMaskFlagsChanged) handler:^NSEvent *(NSEvent *event) {
                VNTKeyboard *strong = weak; if (!strong) return event;
                [strong.lock lock]; BOOL active = keyboardActive(strong);
                if (!active) { [strong.lock unlock]; return event; }
                BOOL down = event.type == NSEventTypeKeyDown;
                if (event.type == NSEventTypeFlagsChanged) {
                    NSEventModifierFlags mask = 0;
                    switch (event.keyCode) {
                        case 54:case 55: mask=NSEventModifierFlagCommand;break;
                        case 56:case 60: mask=NSEventModifierFlagShift;break;
                        case 58:case 61: mask=NSEventModifierFlagOption;break;
                        case 59:case 62: mask=NSEventModifierFlagControl;break;
                        case 57: mask=NSEventModifierFlagCapsLock;break;
                    }
                    down = (event.modifierFlags & mask) != 0;
                }
                if (strong.events.count >= 128) strong.overflow = YES;
                else [strong.events addObject:@{@"key":@(event.keyCode),@"down":@(down)}];
                [strong.lock unlock]; return nil;
            }];
        });
        return (__bridge_retained void *)k;
    }
}
char *vnt_keyboard_read(void *p, int left, int top, int right, int bottom) {
    @autoreleasepool {
        VNTKeyboard *k = (__bridge VNTKeyboard *)p; __block BOOL active = NO;
        dispatch_sync(dispatch_get_main_queue(), ^{
            [k.lock lock]; k.area = NSMakeRect(left,top,right-left,bottom-top); active = keyboardActive(k); [k.lock unlock];
        });
        [k.lock lock];
        NSDictionary *result = @{@"active":@(active),@"overflow":@(k.overflow || !k.monitor),@"events":[k.events copy]}; [k.events removeAllObjects];
        NSData *data = [NSJSONSerialization dataWithJSONObject:result options:0 error:nil]; [k.lock unlock];
        return strndup(data.bytes,data.length);
    }
}
void vnt_keyboard_close(void *p) {
    VNTKeyboard *k = CFBridgingRelease(p);
    dispatch_async(dispatch_get_main_queue(), ^{ [NSEvent removeMonitor:k.monitor]; k.monitor = nil; k.window = nil; });
}
