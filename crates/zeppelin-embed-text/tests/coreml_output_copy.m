// Native regression probe; build with clang -fobjc-arc and CoreML/Foundation.
#import "../objc/ze_coreml.m"
#include <stdio.h>
#include <math.h>

@interface ReadCountingArray : MLMultiArray
@property(nonatomic) size_t boxedReads;
@end
@implementation ReadCountingArray
- (NSNumber *)objectAtIndexedSubscript:(NSInteger)index {
    self.boxedReads += 1;
    return [super objectAtIndexedSubscript:index];
}
@end

int main(void) {
    @autoreleasepool {
        uint16_t bits[] = {0x0000, 0x8000, 0x3c00, 0xc000, 0x0001, 0x03ff, 0x0400, 0x7bff};
        float expected[] = {0.0f, -0.0f, 1.0f, -2.0f, 0x1p-24f, 0x1.ff8p-15f, 0x1p-14f, 65504.0f};
        uint16_t storage[768];
        float out[768];
        for (size_t i = 0; i < 768; i++) storage[i] = bits[i % 8];
        NSError *error = nil;
        ReadCountingArray *array = [[ReadCountingArray alloc]
            initWithDataPointer:storage shape:@[@1, @768]
            dataType:MLMultiArrayDataTypeFloat16 strides:@[@768, @1]
            deallocator:nil error:&error];
        if (!array || error) return 10;
        ze_coreml_copy_embedding(array, out, 768);
        int failed = 0;
        for (size_t i = 0; i < 768; i++) {
            if (memcmp(&out[i], &expected[i % 8], sizeof(float))) {
                fprintf(stderr, "FP16 value mismatch at %zu\n", i);
                return 11;
            }
        }
        if (array.boxedReads != 0) {
            fprintf(stderr, "FAIL FP16 copy used %zu boxed reads, expected zero\n", array.boxedReads);
            failed++;
        }
        float strided[] = {1.0f, 99.0f, -2.0f, 99.0f, 0.5f, 99.0f, -0.0f, 99.0f};
        float logical[] = {1.0f, -2.0f, 0.5f, -0.0f};
        MLMultiArray *noncontiguous = [[MLMultiArray alloc]
            initWithDataPointer:strided shape:@[@1, @4]
            dataType:MLMultiArrayDataTypeFloat32 strides:@[@8, @2]
            deallocator:nil error:&error];
        if (!noncontiguous || error) return 12;
        ze_coreml_copy_embedding(noncontiguous, out, 4);
        if (memcmp(out, logical, sizeof(logical))) {
            fprintf(stderr, "FAIL strided FP32 output copied physical rather than logical elements\n");
            failed++;
        }
        ReadCountingArray *contiguous = [[ReadCountingArray alloc]
            initWithDataPointer:logical shape:@[@1, @4]
            dataType:MLMultiArrayDataTypeFloat32 strides:@[@99, @1]
            deallocator:nil error:&error];
        if (!contiguous || error) return 13;
        ze_coreml_copy_embedding(contiguous, out, 4);
        if (memcmp(out, logical, sizeof(logical)) || contiguous.boxedReads != 0) return 14;
        uint16_t halfStrided[] = {0x3c00, 0, 0xc000, 0, 0, 0, 0x3800, 0, 0x8000, 0};
        ReadCountingArray *halfView = [[ReadCountingArray alloc]
            initWithDataPointer:halfStrided shape:@[@2, @2]
            dataType:MLMultiArrayDataTypeFloat16 strides:@[@6, @2]
            deallocator:nil error:&error];
        if (!halfView || error) return 15;
        ze_coreml_copy_embedding(halfView, out, 4);
        if (memcmp(out, logical, sizeof(logical)) || halfView.boxedReads != 4) return 16;
        fprintf(stderr, "coreml_output_copy: %s; boxed reads=%zu\n", failed ? "FAILED" : "PASS", array.boxedReads);
        return failed ? 1 : 0;
    }
}
