// Standalone native regression test; does not capture pixels or request permissions.
#import "../native.m"
#include <assert.h>

int main(void) {
    @autoreleasepool {
        // Literal keys model the dictionaries supplied by WindowServer.
        assert(sessionIsUnlocked(@{@"kCGSSessionOnConsoleKey": @YES}));
        assert(sessionIsUnlocked(@{@"kCGSSessionOnConsoleKey": @YES, @"CGSSessionScreenIsLocked": @NO}));
        assert(!sessionIsUnlocked(@{@"kCGSSessionOnConsoleKey": @YES, @"CGSSessionScreenIsLocked": @YES}));
        assert(!sessionIsUnlocked(@{@"kCGSSessionOnConsoleKey": @NO}));
        assert(!sessionIsUnlocked(@{}));
        assert(!sessionIsUnlocked(nil));
    }
    return 0;
}
