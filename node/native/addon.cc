#include <node_api.h>

#include <cstdint>
#include <cstring>
#include <exception>
#include <memory>
#include <new>
#include <string>
#include <utility>
#include <vector>

#include "zeppelin_embed.h"

namespace {

struct NativeStore {
  ze_handle handle = 0;
};

bool NapiOk(napi_env env, napi_status status, const char *operation) {
  if (status == napi_ok)
    return true;
  const napi_extended_error_info *info = nullptr;
  napi_get_last_error_info(env, &info);
  const char *detail = info != nullptr && info->error_message != nullptr
                           ? info->error_message
                           : "Node-API call failed";
  std::string message(operation);
  message.append(": ").append(detail);
  napi_throw_error(env, "ERR_ZEPPELIN_NAPI", message.c_str());
  return false;
}

template <typename Function>
napi_value Guard(napi_env env, Function &&function) {
  try {
    return function();
  } catch (const std::bad_alloc &) {
    napi_throw_error(env, "ERR_ZEPPELIN_OUT_OF_MEMORY",
                     "the Node addon could not allocate memory");
  } catch (const std::exception &error) {
    napi_throw_error(env, "ERR_ZEPPELIN_NATIVE", error.what());
  } catch (...) {
    napi_throw_error(env, "ERR_ZEPPELIN_NATIVE",
                     "an unknown native addon failure occurred");
  }
  return nullptr;
}

std::string LastError(ze_handle handle) {
  size_t length = 0;
  if (ze_last_error_message(handle, nullptr, 0, &length) != ZE_OK ||
      length == 0) {
    return "Zeppelin Embed operation failed";
  }
  std::vector<char> buffer(length + 1, '\0');
  size_t written = 0;
  if (ze_last_error_message(handle, buffer.data(), buffer.size(), &written) !=
      ZE_OK) {
    return "Zeppelin Embed operation failed";
  }
  return std::string(buffer.data(), written);
}

napi_value ThrowZeppelin(napi_env env, ze_handle handle, ze_error_code status) {
  const char *code = ze_error_code_name(status);
  const std::string message = LastError(handle);
  napi_value js_message;
  napi_value error;
  napi_value js_name;
  napi_value js_code;
  napi_value js_number;
  if (!NapiOk(env,
              napi_create_string_utf8(env, message.c_str(), message.size(),
                                      &js_message),
              "create error message") ||
      !NapiOk(env, napi_create_error(env, nullptr, js_message, &error),
              "create error") ||
      !NapiOk(env,
              napi_create_string_utf8(env, "ZeppelinError", NAPI_AUTO_LENGTH,
                                      &js_name),
              "create error name") ||
      !NapiOk(env, napi_set_named_property(env, error, "name", js_name),
              "set error name") ||
      !NapiOk(env,
              napi_create_string_utf8(env, code, NAPI_AUTO_LENGTH, &js_code),
              "create error code") ||
      !NapiOk(env, napi_set_named_property(env, error, "code", js_code),
              "set error code") ||
      !NapiOk(env, napi_create_int32(env, status, &js_number),
              "create numeric error code") ||
      !NapiOk(env, napi_set_named_property(env, error, "errorCode", js_number),
              "set numeric error code")) {
    return nullptr;
  }
  napi_throw(env, error);
  return nullptr;
}

napi_value ThrowClosed(napi_env env) {
  napi_value message;
  napi_value error;
  napi_value code;
  napi_value number;
  if (!NapiOk(env,
              napi_create_string_utf8(env, "store is closed", NAPI_AUTO_LENGTH,
                                      &message),
              "create closed message") ||
      !NapiOk(env, napi_create_error(env, nullptr, message, &error),
              "create closed error") ||
      !NapiOk(env,
              napi_create_string_utf8(env, "ZE_ERR_CLOSED", NAPI_AUTO_LENGTH,
                                      &code),
              "create closed code") ||
      !NapiOk(env, napi_set_named_property(env, error, "code", code),
              "set closed code") ||
      !NapiOk(env, napi_create_int32(env, ZE_ERR_CLOSED, &number),
              "create closed numeric code") ||
      !NapiOk(env, napi_set_named_property(env, error, "errorCode", number),
              "set closed numeric code")) {
    return nullptr;
  }
  napi_throw(env, error);
  return nullptr;
}

bool GetNamed(napi_env env, napi_value object, const char *name,
              napi_value *value, bool *present) {
  if (!NapiOk(env, napi_has_named_property(env, object, name, present),
              "inspect property")) {
    return false;
  }
  if (!*present)
    return true;
  return NapiOk(env, napi_get_named_property(env, object, name, value),
                "read property");
}

bool GetUtf8(napi_env env, napi_value value, const char *field,
             std::string *output) {
  napi_valuetype type;
  if (!NapiOk(env, napi_typeof(env, value, &type), "inspect string") ||
      type != napi_string) {
    std::string message(field);
    message.append(" must be a string");
    napi_throw_type_error(env, "ERR_INVALID_ARG_TYPE", message.c_str());
    return false;
  }
  size_t length = 0;
  if (!NapiOk(env, napi_get_value_string_utf8(env, value, nullptr, 0, &length),
              "measure string")) {
    return false;
  }
  output->resize(length);
  size_t written = 0;
  return NapiOk(env,
                napi_get_value_string_utf8(env, value, output->data(),
                                           length + 1, &written),
                "read string");
}

bool GetOptionalString(napi_env env, napi_value object, const char *name,
                       std::string *output, bool *present) {
  napi_value value;
  if (!GetNamed(env, object, name, &value, present))
    return false;
  if (!*present)
    return true;
  return GetUtf8(env, value, name, output);
}

bool GetOptionalBool(napi_env env, napi_value object, const char *name,
                     bool default_value, bool *output) {
  napi_value value;
  bool present = false;
  if (!GetNamed(env, object, name, &value, &present))
    return false;
  if (!present) {
    *output = default_value;
    return true;
  }
  napi_valuetype type;
  if (!NapiOk(env, napi_typeof(env, value, &type), "inspect boolean") ||
      type != napi_boolean) {
    std::string message(name);
    message.append(" must be a boolean");
    napi_throw_type_error(env, "ERR_INVALID_ARG_TYPE", message.c_str());
    return false;
  }
  return NapiOk(env, napi_get_value_bool(env, value, output), "read boolean");
}

bool GetOptionalUint64(napi_env env, napi_value object, const char *name,
                       uint64_t default_value, uint64_t *output) {
  napi_value value;
  bool present = false;
  if (!GetNamed(env, object, name, &value, &present))
    return false;
  if (!present) {
    *output = default_value;
    return true;
  }
  napi_valuetype type;
  bool lossless = false;
  if (!NapiOk(env, napi_typeof(env, value, &type), "inspect bigint") ||
      type != napi_bigint ||
      !NapiOk(env, napi_get_value_bigint_uint64(env, value, output, &lossless),
              "read bigint") ||
      !lossless) {
    std::string message(name);
    message.append(" must be an unsigned 64-bit bigint");
    napi_throw_range_error(env, "ERR_OUT_OF_RANGE", message.c_str());
    return false;
  }
  return true;
}

bool ParseEnum(const std::string &value, const char *const *names, size_t count,
               int32_t *output) {
  for (size_t index = 0; index < count; ++index) {
    if (value == names[index]) {
      *output = static_cast<int32_t>(index);
      return true;
    }
  }
  return false;
}

bool GetFloat32Array(napi_env env, napi_value value, const char *field,
                     const float **data, size_t *length) {
  bool is_typed_array = false;
  if (!NapiOk(env, napi_is_typedarray(env, value, &is_typed_array),
              "inspect typed array") ||
      !is_typed_array) {
    std::string message(field);
    message.append(" must be a Float32Array");
    napi_throw_type_error(env, "ERR_INVALID_ARG_TYPE", message.c_str());
    return false;
  }
  napi_typedarray_type type;
  napi_value array_buffer;
  size_t byte_offset = 0;
  void *raw = nullptr;
  if (!NapiOk(env,
              napi_get_typedarray_info(env, value, &type, length, &raw,
                                       &array_buffer, &byte_offset),
              "read typed array") ||
      type != napi_float32_array) {
    std::string message(field);
    message.append(" must be a Float32Array");
    napi_throw_type_error(env, "ERR_INVALID_ARG_TYPE", message.c_str());
    return false;
  }
  *data = static_cast<const float *>(raw);
  return true;
}

bool GetDocId(napi_env env, napi_value value, ZeDocId *output) {
  napi_valuetype type;
  if (!NapiOk(env, napi_typeof(env, value, &type), "inspect document id") ||
      type != napi_bigint) {
    napi_throw_type_error(env, "ERR_INVALID_ARG_TYPE",
                          "document id must be a bigint");
    return false;
  }
  int sign = 0;
  size_t count = 2;
  uint64_t words[2] = {0, 0};
  if (!NapiOk(env,
              napi_get_value_bigint_words(env, value, &sign, &count, words),
              "read document id") ||
      sign != 0 || count > 2) {
    napi_throw_range_error(env, "ERR_OUT_OF_RANGE",
                           "document id must be an unsigned 128-bit bigint");
    return false;
  }
  output->low = words[0];
  output->high = count == 2 ? words[1] : 0;
  return true;
}

bool GetOptionalBigUint64(napi_env env, napi_value object, const char *name,
                          uint64_t default_value, uint64_t *output) {
  return GetOptionalUint64(env, object, name, default_value, output);
}

bool GetOptionalBigInt64(napi_env env, napi_value object, const char *name,
                         int64_t default_value, int64_t *output) {
  napi_value value;
  bool present = false;
  if (!GetNamed(env, object, name, &value, &present))
    return false;
  if (!present) {
    *output = default_value;
    return true;
  }
  napi_valuetype type;
  bool lossless = false;
  if (!NapiOk(env, napi_typeof(env, value, &type), "inspect bigint") ||
      type != napi_bigint ||
      !NapiOk(env, napi_get_value_bigint_int64(env, value, output, &lossless),
              "read bigint") ||
      !lossless) {
    std::string message(name);
    message.append(" must be a signed 64-bit bigint");
    napi_throw_range_error(env, "ERR_OUT_OF_RANGE", message.c_str());
    return false;
  }
  return true;
}

bool SetNamed(napi_env env, napi_value object, const char *name,
              napi_value value) {
  return NapiOk(env, napi_set_named_property(env, object, name, value),
                "set result property");
}

bool CreateUint128(napi_env env, ZeDocId id, napi_value *output) {
  const uint64_t words[2] = {id.low, id.high};
  return NapiOk(env, napi_create_bigint_words(env, 0, 2, words, output),
                "create document id");
}

NativeStore *UnwrapStore(napi_env env, napi_value receiver) {
  NativeStore *store = nullptr;
  if (!NapiOk(env,
              napi_unwrap(env, receiver, reinterpret_cast<void **>(&store)),
              "unwrap store")) {
    return nullptr;
  }
  if (store == nullptr || store->handle == 0) {
    ThrowClosed(env);
    return nullptr;
  }
  return store;
}

void FinalizeStore(napi_env, void *data, void *) {
  auto *store = static_cast<NativeStore *>(data);
  if (store != nullptr) {
    if (store->handle != 0)
      ze_close(store->handle);
    delete store;
  }
}

napi_value ConstructStore(napi_env env, napi_callback_info info) {
  return Guard(env, [&]() -> napi_value {
    size_t argc = 2;
    napi_value args[2];
    napi_value receiver;
    if (!NapiOk(env,
                napi_get_cb_info(env, info, &argc, args, &receiver, nullptr),
                "read constructor arguments")) {
      return nullptr;
    }
    if (argc < 1) {
      napi_throw_type_error(env, "ERR_MISSING_ARGS", "path is required");
      return nullptr;
    }
    std::string path;
    if (!GetUtf8(env, args[0], "path", &path))
      return nullptr;

    napi_value options;
    if (argc < 2) {
      if (!NapiOk(env, napi_create_object(env, &options), "create options"))
        return nullptr;
    } else {
      options = args[1];
      napi_valuetype type;
      if (!NapiOk(env, napi_typeof(env, options, &type), "inspect options") ||
          type != napi_object) {
        napi_throw_type_error(env, "ERR_INVALID_ARG_TYPE",
                              "options must be an object");
        return nullptr;
      }
    }

    bool read_only = false;
    uint64_t drain_ms = 5000;
    uint64_t resident_bytes = 512ULL * 1024 * 1024;
    uint64_t temp_bytes = 512ULL * 1024 * 1024;
    if (!GetOptionalBool(env, options, "readOnly", false, &read_only) ||
        !GetOptionalUint64(env, options, "readerDrainTimeoutMs", drain_ms,
                           &drain_ms) ||
        !GetOptionalUint64(env, options, "maxResidentBytes", resident_bytes,
                           &resident_bytes) ||
        !GetOptionalUint64(env, options, "maxTempBytes", temp_bytes,
                           &temp_bytes)) {
      return nullptr;
    }

    int32_t durability = 0;
    int32_t commit_tier = 0;
    std::string value;
    bool present = false;
    if (!GetOptionalString(env, options, "durability", &value, &present))
      return nullptr;
    if (present) {
      const char *names[] = {"derived", "durable", "attached"};
      if (!ParseEnum(value, names, 3, &durability)) {
        napi_throw_range_error(
            env, "ERR_OUT_OF_RANGE",
            "durability must be derived, durable, or attached");
        return nullptr;
      }
    }
    value.clear();
    present = false;
    if (!GetOptionalString(env, options, "commitTier", &value, &present))
      return nullptr;
    if (present) {
      const char *names[] = {"none", "ordered", "durable"};
      if (!ParseEnum(value, names, 3, &commit_tier)) {
        napi_throw_range_error(env, "ERR_OUT_OF_RANGE",
                               "commitTier must be none, ordered, or durable");
        return nullptr;
      }
    }

    ZeOpenRequest request{};
    request.abi_size = sizeof(request);
    request.path = reinterpret_cast<const uint8_t *>(path.data());
    request.path_len = path.size();
    request.access_mode = read_only ? 1 : 0;
    request.durability_mode = durability;
    request.commit_tier = commit_tier;
    request.reader_drain_timeout_ms = drain_ms;
    request.max_resident_bytes = resident_bytes;
    request.max_temp_bytes = temp_bytes;

    ze_handle handle = 0;
    const ze_error_code status = ze_open(&request, &handle);
    if (status != ZE_OK)
      return ThrowZeppelin(env, 0, status);

    auto store = std::make_unique<NativeStore>(NativeStore{handle});
    if (!NapiOk(env,
                napi_wrap(env, receiver, store.get(), FinalizeStore, nullptr,
                          nullptr),
                "attach store handle")) {
      ze_close(handle);
      return nullptr;
    }
    store.release();
    return receiver;
  });
}

napi_value CloseStore(napi_env env, napi_callback_info info) {
  return Guard(env, [&]() -> napi_value {
    size_t argc = 0;
    napi_value receiver;
    if (!NapiOk(env,
                napi_get_cb_info(env, info, &argc, nullptr, &receiver, nullptr),
                "read close receiver")) {
      return nullptr;
    }
    NativeStore *store = UnwrapStore(env, receiver);
    if (store == nullptr)
      return nullptr;
    const ze_handle handle = store->handle;
    const ze_error_code status = ze_close(handle);
    if (status == ZE_OK || status == ZE_ERR_POISONED)
      store->handle = 0;
    if (status != ZE_OK)
      return ThrowZeppelin(env, handle, status);
    napi_value undefined;
    if (!NapiOk(env, napi_get_undefined(env, &undefined), "create undefined"))
      return nullptr;
    return undefined;
  });
}

napi_value Ingest(napi_env env, napi_callback_info info) {
  return Guard(env, [&]() -> napi_value {
    size_t argc = 2;
    napi_value args[2];
    napi_value receiver;
    if (!NapiOk(env,
                napi_get_cb_info(env, info, &argc, args, &receiver, nullptr),
                "read ingest arguments")) {
      return nullptr;
    }
    if (argc < 2) {
      napi_throw_type_error(env, "ERR_MISSING_ARGS",
                            "documents and dimension are required");
      return nullptr;
    }
    NativeStore *store = UnwrapStore(env, receiver);
    if (store == nullptr)
      return nullptr;

    bool is_array = false;
    uint32_t document_count = 0;
    uint32_t dimension = 0;
    if (!NapiOk(env, napi_is_array(env, args[0], &is_array),
                "inspect documents") ||
        !is_array) {
      napi_throw_type_error(env, "ERR_INVALID_ARG_TYPE",
                            "documents must be an array");
      return nullptr;
    }
    if (!NapiOk(env, napi_get_array_length(env, args[0], &document_count),
                "read document count") ||
        !NapiOk(env, napi_get_value_uint32(env, args[1], &dimension),
                "read dimension")) {
      return nullptr;
    }

    std::vector<ZeIngestDocument> documents(document_count);
    for (uint32_t index = 0; index < document_count; ++index) {
      napi_value document;
      if (!NapiOk(env, napi_get_element(env, args[0], index, &document),
                  "read document")) {
        return nullptr;
      }
      napi_value id;
      napi_value vector;
      bool present = false;
      if (!GetNamed(env, document, "id", &id, &present) || !present) {
        napi_throw_type_error(env, "ERR_MISSING_ARGS",
                              "each document requires id");
        return nullptr;
      }
      if (!GetNamed(env, document, "vector", &vector, &present) || !present) {
        napi_throw_type_error(env, "ERR_MISSING_ARGS",
                              "each document requires vector");
        return nullptr;
      }
      ZeIngestDocument &native = documents[index];
      native = ZeIngestDocument{};
      native.abi_size = sizeof(native);
      if (!GetDocId(env, id, &native.doc_id) ||
          !GetOptionalBigUint64(env, document, "revision", 1,
                                &native.revision) ||
          !GetOptionalBigInt64(env, document, "timestamp", 0,
                               &native.timestamp) ||
          !GetFloat32Array(env, vector, "document vector", &native.vector,
                           &native.vector_len)) {
        return nullptr;
      }
    }

    ZeIngestRequest request{};
    request.abi_size = sizeof(request);
    request.documents = documents.data();
    request.document_count = documents.size();
    request.dimension = dimension;
    ZeMutationReport report{};
    report.abi_size = sizeof(report);
    const ze_error_code status = ze_ingest(store->handle, &request, &report);
    if (status != ZE_OK)
      return ThrowZeppelin(env, store->handle, status);

    napi_value result;
    napi_value sequence;
    napi_value generation;
    if (!NapiOk(env, napi_create_object(env, &result),
                "create mutation report") ||
        !NapiOk(env, napi_create_bigint_uint64(env, report.sequence, &sequence),
                "create sequence") ||
        !SetNamed(env, result, "sequence", sequence) ||
        !NapiOk(env,
                napi_create_bigint_uint64(env, report.generation, &generation),
                "create generation") ||
        !SetNamed(env, result, "generation", generation)) {
      return nullptr;
    }
    return result;
  });
}

napi_value Search(napi_env env, napi_callback_info info) {
  return Guard(env, [&]() -> napi_value {
    size_t argc = 2;
    napi_value args[2];
    napi_value receiver;
    if (!NapiOk(env,
                napi_get_cb_info(env, info, &argc, args, &receiver, nullptr),
                "read search arguments")) {
      return nullptr;
    }
    if (argc < 2) {
      napi_throw_type_error(env, "ERR_MISSING_ARGS",
                            "vector and k are required");
      return nullptr;
    }
    NativeStore *store = UnwrapStore(env, receiver);
    if (store == nullptr)
      return nullptr;

    const float *vector = nullptr;
    size_t length = 0;
    uint32_t k = 0;
    if (!GetFloat32Array(env, args[0], "query vector", &vector, &length) ||
        !NapiOk(env, napi_get_value_uint32(env, args[1], &k), "read k")) {
      return nullptr;
    }

    ZeSearchRequest request{};
    request.abi_size = sizeof(request);
    request.vector = vector;
    request.vector_len = length;
    request.dimension = length;
    request.k = k;
    ZeSearchResult result{};
    result.abi_size = sizeof(result);
    const ze_error_code status = ze_search(store->handle, &request, &result);
    if (status != ZE_OK)
      return ThrowZeppelin(env, store->handle, status);

    napi_value hits;
    if (!NapiOk(env,
                napi_create_array_with_length(env, result.hit_count, &hits),
                "create hit array")) {
      ze_search_result_free(&result);
      return nullptr;
    }
    for (size_t index = 0; index < result.hit_count; ++index) {
      const ZeSearchHit &native = result.hits[index];
      if (native.has_document == 0) {
        ze_search_result_free(&result);
        napi_throw_error(env, "ERR_ZEPPELIN_NATIVE",
                         "search returned a hit without a document id");
        return nullptr;
      }
      napi_value hit;
      napi_value id;
      napi_value revision;
      napi_value score;
      if (!NapiOk(env, napi_create_object(env, &hit), "create search hit") ||
          !CreateUint128(env, native.doc_id, &id) ||
          !SetNamed(env, hit, "id", id) ||
          !NapiOk(env,
                  napi_create_bigint_uint64(env, native.revision, &revision),
                  "create revision") ||
          !SetNamed(env, hit, "revision", revision) ||
          !NapiOk(env, napi_create_double(env, native.score, &score),
                  "create score") ||
          !SetNamed(env, hit, "score", score) ||
          !NapiOk(env, napi_set_element(env, hits, index, hit),
                  "append search hit")) {
        ze_search_result_free(&result);
        return nullptr;
      }
    }
    const ze_error_code free_status = ze_search_result_free(&result);
    if (free_status != ZE_OK)
      return ThrowZeppelin(env, store->handle, free_status);
    return hits;
  });
}

napi_value Initialize(napi_env env, napi_value exports) {
  napi_property_descriptor methods[] = {
      {"ingest", nullptr, Ingest, nullptr, nullptr, nullptr, napi_default,
       nullptr},
      {"search", nullptr, Search, nullptr, nullptr, nullptr, napi_default,
       nullptr},
      {"close", nullptr, CloseStore, nullptr, nullptr, nullptr, napi_default,
       nullptr},
  };
  napi_value constructor;
  if (!NapiOk(env,
              napi_define_class(
                  env, "NativeStore", NAPI_AUTO_LENGTH, ConstructStore, nullptr,
                  sizeof(methods) / sizeof(methods[0]), methods, &constructor),
              "define NativeStore") ||
      !SetNamed(env, exports, "NativeStore", constructor)) {
    return nullptr;
  }
  napi_value abi_version;
  if (!NapiOk(env, napi_create_uint32(env, ze_abi_version(), &abi_version),
              "create ABI version") ||
      !SetNamed(env, exports, "abiVersion", abi_version)) {
    return nullptr;
  }
  return exports;
}

} // namespace

NAPI_MODULE(NODE_GYP_MODULE_NAME, Initialize)
