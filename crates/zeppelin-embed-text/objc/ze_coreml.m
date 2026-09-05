// Minimal CoreML bridge for the query tower.
//
// The engine owns tokenization, pooling policy and normalisation; this
// shim only evaluates one already-padded token row and copies the
// embedding out. It never allocates anything the caller does not free
// through ze_coreml_close, and it converts every Objective-C error into
// a C string rather than letting an exception cross the FFI boundary.

#import <CoreML/CoreML.h>
#import <Foundation/Foundation.h>
#include <stdint.h>
#include <string.h>

struct ze_coreml_model {
    // Held as opaque pointers so ARC ownership crosses the C boundary
    // through explicit bridge casts rather than implicitly.
    void *model;
    void *options;
};

static char *ze_copy_error(NSError *error, const char *fallback) {
    const char *text = fallback;
    if (error != nil) {
        text = [[error localizedDescription] UTF8String];
        if (text == NULL) {
            text = fallback;
        }
    }
    size_t length = strlen(text) + 1;
    char *copy = (char *)malloc(length);
    if (copy != NULL) {
        memcpy(copy, text, length);
    }
    return copy;
}

void ze_coreml_string_free(char *text) {
    free(text);
}

// compute_units: 0 = CPU only, 1 = CPU and GPU, 2 = CPU and Neural Engine,
// 3 = all. Returns NULL and sets *error_out on failure.
struct ze_coreml_model *ze_coreml_open(const char *path, int32_t compute_units, char **error_out) {
    @autoreleasepool {
        if (path == NULL) {
            if (error_out) *error_out = ze_copy_error(nil, "model path is null");
            return NULL;
        }
        NSString *string = [NSString stringWithUTF8String:path];
        NSURL *url = [NSURL fileURLWithPath:string];
        MLModelConfiguration *configuration = [[MLModelConfiguration alloc] init];
        switch (compute_units) {
            case 0: configuration.computeUnits = MLComputeUnitsCPUOnly; break;
            case 1: configuration.computeUnits = MLComputeUnitsCPUAndGPU; break;
            case 2: configuration.computeUnits = MLComputeUnitsCPUAndNeuralEngine; break;
            default: configuration.computeUnits = MLComputeUnitsAll; break;
        }
        NSError *error = nil;
        MLModel *model = [MLModel modelWithContentsOfURL:url configuration:configuration error:&error];
        if (model == nil) {
            if (error_out) *error_out = ze_copy_error(error, "CoreML model failed to load");
            return NULL;
        }
        struct ze_coreml_model *handle = (struct ze_coreml_model *)calloc(1, sizeof(struct ze_coreml_model));
        if (handle == NULL) {
            if (error_out) *error_out = ze_copy_error(nil, "out of memory");
            return NULL;
        }
        handle->model = (__bridge_retained void *)model;
        handle->options = (__bridge_retained void *)[[MLPredictionOptions alloc] init];
        return handle;
    }
}

static void ze_coreml_copy_embedding(MLMultiArray *embedding, float *out, size_t out_len) {
    __block BOOL copied = NO;
    MLMultiArrayDataType type = embedding.dataType;
    if (@available(macOS 12.3, *)) {
        if (type == MLMultiArrayDataTypeFloat16 || type == MLMultiArrayDataTypeFloat32) {
            [embedding getBytesWithHandler:^(const void *bytes, NSInteger size) {
                // A logical embedding may be a strided view. Only contiguous
                // rows may use the bulk path; singleton strides are irrelevant.
                NSArray<NSNumber *> *shape = embedding.shape;
                NSArray<NSNumber *> *strides = embedding.strides;
                if (shape.count != strides.count || size < 0) return;
                size_t expected = 1;
                for (NSUInteger axis = shape.count; axis > 0; axis -= 1) {
                    size_t extent = [shape[axis - 1] unsignedLongLongValue];
                    size_t stride = [strides[axis - 1] unsignedLongLongValue];
                    if (extent == 0 || (extent > 1 && stride != expected)) return;
                    if (expected > SIZE_MAX / extent) return;
                    expected *= extent;
                }
                size_t width = type == MLMultiArrayDataTypeFloat16 ? sizeof(__fp16) : sizeof(float);
                if (out_len > (size_t)size / width) return;
                if (type == MLMultiArrayDataTypeFloat32) {
                    memcpy(out, bytes, out_len * sizeof(float));
                } else {
                    const __fp16 *values = bytes;
                    for (size_t index = 0; index < out_len; index += 1) {
                        out[index] = (float)values[index];
                    }
                }
                copied = YES;
            }];
        }
    }
    if (copied) return;
    for (size_t index = 0; index < out_len; index += 1) {
        out[index] = [[embedding objectAtIndexedSubscript:(NSInteger)index] floatValue];
    }
}

// Evaluates one row. `ids` and `mask` each hold `sequence` int32 values.
// `out` receives `out_len` floats. Returns 0 on success.
int32_t ze_coreml_predict(struct ze_coreml_model *handle,
                          const int32_t *ids,
                          const int32_t *mask,
                          size_t sequence,
                          float *out,
                          size_t out_len,
                          char **error_out) {
    @autoreleasepool {
        if (handle == NULL || ids == NULL || mask == NULL || out == NULL) {
            if (error_out) *error_out = ze_copy_error(nil, "null argument");
            return 1;
        }
        NSError *error = nil;
        NSArray<NSNumber *> *shape = @[@1, @((NSInteger)sequence)];
        MLMultiArray *idArray = [[MLMultiArray alloc] initWithShape:shape
                                                          dataType:MLMultiArrayDataTypeInt32
                                                             error:&error];
        if (idArray == nil) {
            if (error_out) *error_out = ze_copy_error(error, "input_ids allocation failed");
            return 2;
        }
        MLMultiArray *maskArray = [[MLMultiArray alloc] initWithShape:shape
                                                             dataType:MLMultiArrayDataTypeInt32
                                                                error:&error];
        if (maskArray == nil) {
            if (error_out) *error_out = ze_copy_error(error, "attention_mask allocation failed");
            return 3;
        }
        memcpy(idArray.dataPointer, ids, sequence * sizeof(int32_t));
        memcpy(maskArray.dataPointer, mask, sequence * sizeof(int32_t));

        MLDictionaryFeatureProvider *input =
            [[MLDictionaryFeatureProvider alloc] initWithDictionary:@{
                @"input_ids": idArray,
                @"attention_mask": maskArray,
            } error:&error];
        if (input == nil) {
            if (error_out) *error_out = ze_copy_error(error, "feature provider failed");
            return 4;
        }
        MLModel *model = (__bridge MLModel *)handle->model;
        MLPredictionOptions *options = (__bridge MLPredictionOptions *)handle->options;
        id<MLFeatureProvider> result = [model predictionFromFeatures:input options:options error:&error];
        if (result == nil) {
            if (error_out) *error_out = ze_copy_error(error, "CoreML prediction failed");
            return 5;
        }
        MLFeatureValue *value = [result featureValueForName:@"embedding"];
        MLMultiArray *embedding = value.multiArrayValue;
        if (embedding == nil) {
            if (error_out) *error_out = ze_copy_error(nil, "output embedding is absent");
            return 6;
        }
        if ((size_t)embedding.count != out_len) {
            if (error_out) *error_out = ze_copy_error(nil, "output embedding has an unexpected width");
            return 7;
        }
        ze_coreml_copy_embedding(embedding, out, out_len);
        return 0;
    }
}

void ze_coreml_close(struct ze_coreml_model *handle) {
    if (handle == NULL) {
        return;
    }
    if (handle->model != NULL) {
        MLModel *model = (__bridge_transfer MLModel *)handle->model;
        (void)model;
    }
    if (handle->options != NULL) {
        MLPredictionOptions *options = (__bridge_transfer MLPredictionOptions *)handle->options;
        (void)options;
    }
    free(handle);
}
