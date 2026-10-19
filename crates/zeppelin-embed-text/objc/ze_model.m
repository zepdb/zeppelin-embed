// ZE-501: Foundation transport and libSystem SHA-256, no runtime dependency.
#import <Foundation/Foundation.h>
#include <CommonCrypto/CommonDigest.h>
#include <errno.h>
#include <fcntl.h>
#include <unistd.h>

// One authorization predicate, shared by initial requests and every redirect.
int ze_model_token_allowed(const char *url, const char *authorized_host) {
    @autoreleasepool {
        if (url == NULL || authorized_host == NULL) { return 0; }
        NSURL *location = [NSURL URLWithString:[NSString stringWithUTF8String:url]];
        NSString *host = [NSString stringWithUTF8String:authorized_host];
        return [location.scheme.lowercaseString isEqualToString:@"https"] &&
               host.length != 0 && [location.host.lowercaseString isEqualToString:host.lowercaseString];
    }
}

@interface ZEModelDownload : NSObject <NSURLSessionDataDelegate> {
@public
    CC_SHA256_CTX hash;
    int fd;
    uint64_t bytes;
    uint64_t limit;
    int result;
    BOOL allowHTTP;
    NSString *failure;
    NSString *authorizedHost;
    NSString *authorization;
    dispatch_semaphore_t finished;
}
@end

@implementation ZEModelDownload
- (instancetype)init {
    self = [super init];
    if (self) { fd = -1; }
    return self;
}
- (void)dealloc { if (fd >= 0) { close(fd); } }
- (void)URLSession:(NSURLSession *)session dataTask:(NSURLSessionDataTask *)task
    didReceiveResponse:(NSURLResponse *)response
    completionHandler:(void (^)(NSURLSessionResponseDisposition))completionHandler {
    (void)session; (void)task;
    if (![response isKindOfClass:[NSHTTPURLResponse class]]) {
        result = -1; failure = @"not an HTTP response";
    } else {
        NSInteger status = ((NSHTTPURLResponse *)response).statusCode;
        if (status < 200 || status >= 300) {
            result = -1; failure = [NSString stringWithFormat:@"HTTP %ld", (long)status];
        }
    }
    completionHandler(result == 0 ? NSURLSessionResponseAllow : NSURLSessionResponseCancel);
}
- (void)URLSession:(NSURLSession *)session task:(NSURLSessionTask *)task
    willPerformHTTPRedirection:(NSHTTPURLResponse *)response newRequest:(NSURLRequest *)request
    completionHandler:(void (^)(NSURLRequest *))completionHandler {
    (void)session; (void)task; (void)response;
    NSMutableURLRequest *redirect = [request mutableCopy];
    // Rebuild this header at every hop, including when Foundation has removed
    // it, so only the single predicate controls token forwarding.
    BOOL allowed = ze_model_token_allowed(redirect.URL.absoluteString.UTF8String, authorizedHost.UTF8String);
    [redirect setValue:allowed ? authorization : nil forHTTPHeaderField:@"Authorization"];
    NSString *scheme = redirect.URL.scheme.lowercaseString;
    if (![scheme isEqualToString:@"https"] && !(allowHTTP && [scheme isEqualToString:@"http"])) {
        result = -1; failure = @"redirect violates HTTPS policy";
        completionHandler(nil);
    } else { completionHandler(redirect); }
}
- (void)URLSession:(NSURLSession *)session dataTask:(NSURLSessionDataTask *)task didReceiveData:(NSData *)data {
    (void)session;
    if (result != 0) { return; }
    if (data.length > limit - bytes) {
        result = -2; failure = @"download exceeds pinned size limit";
        [task cancel]; return;
    }
    const unsigned char *cursor = data.bytes;
    NSUInteger left = data.length;
    while (left != 0) {
        // Keep each CommonCrypto update within its 32-bit length domain.
        size_t chunk = left > UINT32_MAX ? UINT32_MAX : left;
        ssize_t written = write(fd, cursor, chunk);
        if (written < 0 && errno == EINTR) { continue; }
        if (written <= 0) {
            result = -1; failure = @"cannot write model download";
            [task cancel]; return;
        }
        CC_SHA256_Update(&hash, cursor, (CC_LONG)written);
        bytes += (uint64_t)written;
        cursor += written; left -= (NSUInteger)written;
    }
}
- (void)URLSession:(NSURLSession *)session task:(NSURLSessionTask *)task didCompleteWithError:(NSError *)error {
    (void)session; (void)task;
    if (error != nil && result == 0) { result = -1; failure = error.localizedDescription; }
    dispatch_semaphore_signal(finished);
}
@end

// Output buffers are caller-owned: digest[32], error[error_cap].
int ze_model_download(const char *url, const char *path, const char *token,
                      const char *authorized_endpoint, int allow_http,
                      uint64_t limit, unsigned char *digest, uint64_t *bytes,
                      char *error, size_t error_cap) {
    @autoreleasepool {
        ZEModelDownload *delegate = nil;
        NSURLSession *session = nil;
        @try {
            NSURL *location = [NSURL URLWithString:[NSString stringWithUTF8String:url]];
            NSString *scheme = location.scheme.lowercaseString;
            if (location.host.length == 0 ||
                (![scheme isEqualToString:@"https"] && !(allow_http && [scheme isEqualToString:@"http"]))) {
                snprintf(error, error_cap, "invalid HTTP(S) model URL"); return -1;
            }
            delegate = [ZEModelDownload new];
            delegate->fd = open(path, O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW, 0600);
            if (delegate->fd < 0) { snprintf(error, error_cap, "cannot create model download: %s", strerror(errno)); return -1; }
            delegate->limit = limit;
            delegate->allowHTTP = allow_http != 0;
            delegate->authorizedHost = [NSURL URLWithString:[NSString stringWithUTF8String:authorized_endpoint]].host;
            if (token != NULL) {
                delegate->authorization = [@"Bearer " stringByAppendingString:[NSString stringWithUTF8String:token]];
            }
            delegate->finished = dispatch_semaphore_create(0);
            CC_SHA256_Init(&delegate->hash);
            NSURLSessionConfiguration *config = [NSURLSessionConfiguration ephemeralSessionConfiguration];
            config.URLCache = nil;
            config.HTTPCookieStorage = nil;
            config.URLCredentialStorage = nil;
            config.requestCachePolicy = NSURLRequestReloadIgnoringLocalCacheData;
            config.timeoutIntervalForRequest = 60;
            config.timeoutIntervalForResource = 3600;
            NSOperationQueue *queue = [NSOperationQueue new];
            queue.maxConcurrentOperationCount = 1;
            session = [NSURLSession sessionWithConfiguration:config delegate:delegate delegateQueue:queue];
            NSMutableURLRequest *request = [NSMutableURLRequest requestWithURL:location];
            [request setValue:@"identity" forHTTPHeaderField:@"Accept-Encoding"];
            BOOL allowed = ze_model_token_allowed(url, delegate->authorizedHost.UTF8String);
            [request setValue:allowed ? delegate->authorization : nil forHTTPHeaderField:@"Authorization"];
            [[session dataTaskWithRequest:request] resume];
            dispatch_semaphore_wait(delegate->finished, DISPATCH_TIME_FOREVER);
            [session finishTasksAndInvalidate];
            int result = delegate->result;
            if (result == 0 && fsync(delegate->fd) != 0) { result = -1; delegate->failure = @"cannot sync model download"; }
            int fd = delegate->fd;
            delegate->fd = -1;
            if (close(fd) != 0 && result == 0) { result = -1; delegate->failure = @"cannot close model download"; }
            *bytes = delegate->bytes;
            if (result == 0) { CC_SHA256_Final(digest, &delegate->hash); }
            else { snprintf(error, error_cap, "%s", delegate->failure.UTF8String ?: "model download failed"); }
            return result;
        } @catch (NSException *exception) {
            [session invalidateAndCancel];
            snprintf(error, error_cap, "%s", exception.reason.UTF8String ?: "model download exception"); return -1;
        }
    }
}

int ze_model_file_sha256(const char *path, unsigned char *digest, uint64_t *bytes) {
    int fd = open(path, O_RDONLY | O_NOFOLLOW);
    if (fd < 0) { return -1; }
    CC_SHA256_CTX hash;
    CC_SHA256_Init(&hash);
    unsigned char buffer[65536];
    *bytes = 0;
    for (;;) {
        ssize_t length = read(fd, buffer, sizeof(buffer));
        if (length < 0 && errno == EINTR) { continue; }
        if (length < 0) { close(fd); return -1; }
        if (length == 0) { break; }
        CC_SHA256_Update(&hash, buffer, (CC_LONG)length);
        *bytes += (uint64_t)length;
    }
    if (close(fd) != 0) { return -1; }
    CC_SHA256_Final(digest, &hash);
    return 0;
}
