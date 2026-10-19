// Exercise the real shim at CoreML's asynchronous plan-loading boundary.
#import <CoreML/CoreML.h>
#import <objc/runtime.h>
#import <dispatch/dispatch.h>
#include <assert.h>
#include <string.h>

// Assert the production deadline, then expire immediately for deterministic tests.
static long ze_test_plan_wait(dispatch_semaphore_t ready, dispatch_time_t deadline) {
    assert(deadline != DISPATCH_TIME_FOREVER && "compute plan wait must be bounded");
    return dispatch_semaphore_wait(ready, DISPATCH_TIME_NOW);
}
#define dispatch_semaphore_wait ze_test_plan_wait
#include "../../objc/ze_coreml.m"
#undef dispatch_semaphore_wait

static void (^pending)(MLComputePlan *, NSError *);
static BOOL fail_plan;
static BOOL unknown_device;

static void ze_test_unexpected_plan(id self, SEL selector, NSURL *url,
                                    MLModelConfiguration *configuration,
                                    void (^completion)(MLComputePlan *, NSError *)) {
    assert(0 && "model open must not load a compute plan");
}

@interface ZEFutureDeviceUsage : NSObject
@end
@implementation ZEFutureDeviceUsage
- (id<MLComputeDeviceProtocol>)preferredComputeDevice {
    return (id<MLComputeDeviceProtocol>)self;
}
@end

// A one-layer plan whose preferred device is unknown to the current SDK.
@interface ZEFuturePlan : NSObject
@end
@implementation ZEFuturePlan
- (id)modelStructure { return self; }
- (id)neuralNetwork { return self; }
- (NSArray *)layers { return @[[NSObject new]]; }
- (MLComputePlanDeviceUsage *)computeDeviceUsageForNeuralNetworkLayer:(id)layer {
    return (MLComputePlanDeviceUsage *)[ZEFutureDeviceUsage new];
}
@end

static void ze_test_plan(id self, SEL selector, NSURL *url,
                         MLModelConfiguration *configuration,
                         void (^completion)(MLComputePlan *, NSError *)) {
    if (fail_plan) {
        completion(nil, [NSError errorWithDomain:@"ZE516" code:1
                                       userInfo:@{NSLocalizedDescriptionKey: @"plan load failure"}]);
    } else if (unknown_device) {
        completion((MLComputePlan *)[ZEFuturePlan new], nil);
    } else {
        pending = completion;
    }
}

int main(int argc, const char **argv) {
    @autoreleasepool {
        if (@available(macOS 14.4, *)) {
            if (argc == 3 && strcmp(argv[1], "open") == 0) {
                method_setImplementation(class_getClassMethod([MLComputePlan class],
                    @selector(loadContentsOfURL:configuration:completionHandler:)), (IMP)ze_test_unexpected_plan);
                char *error = NULL;
                struct ze_coreml_model *model = ze_coreml_open(argv[2], 0, &error);
                assert(model != NULL && error == NULL);
                ze_coreml_close(model);
                return 0;
            }
            assert(argc == 2);
            fail_plan = strcmp(argv[1], "failure") == 0;
            unknown_device = strcmp(argv[1], "unknown") == 0;
            method_setImplementation(class_getClassMethod([MLComputePlan class],
                @selector(loadContentsOfURL:configuration:completionHandler:)), (IMP)ze_test_plan);
            struct ze_coreml_compute_units counts = {0};
            int32_t has_observed = 0;
            char *error = NULL;
            int32_t status = ze_coreml_compute_plan("/unused.mlmodelc", 0, &counts, &has_observed, &error);
            if (unknown_device) {
                assert(status == 0 && has_observed == 1 && error == NULL);
                assert(counts.cpu_ops == 0 && counts.gpu_ops == 0 && counts.neural_engine_ops == 0);
                assert(counts.other_ops == 1);
            } else {
                assert(status != 0 && has_observed == 0 && error != NULL);
                assert(strcmp(error, fail_plan ? "plan load failure" :
                    "CoreML compute plan timed out after 30 seconds") == 0);
                ze_coreml_string_free(error);
                // A late callback must remain safe after the lazy call returns.
                if (pending) pending(nil, nil);
                pending = nil;
            }
        } else {
            return 77;
        }
    }
}
