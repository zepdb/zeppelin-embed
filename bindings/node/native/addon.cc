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

template <typename Result, ze_error_code (*Free)(Result *)> class ResultOwner {
public:
  explicit ResultOwner(Result *result) : result_(result) {}
  ResultOwner(const ResultOwner &) = delete;
  ResultOwner &operator=(const ResultOwner &) = delete;
  ~ResultOwner() {
    if (result_ != nullptr)
      Free(result_);
  }

  ze_error_code FreeNow() {
    Result *result = result_;
    result_ = nullptr;
    return Free(result);
  }

private:
  Result *result_;
};

bool GetUint8ArrayCopy(napi_env env, napi_value value, const char *field,
                       std::vector<uint8_t> *output) {
  bool is_typed_array = false;
  if (!NapiOk(env, napi_is_typedarray(env, value, &is_typed_array),
              "inspect typed array") ||
      !is_typed_array) {
    std::string message(field);
    message.append(" must be a Uint8Array");
    napi_throw_type_error(env, "ERR_INVALID_ARG_TYPE", message.c_str());
    return false;
  }
  napi_typedarray_type type;
  size_t length = 0;
  void *raw = nullptr;
  napi_value array_buffer;
  size_t byte_offset = 0;
  if (!NapiOk(env,
              napi_get_typedarray_info(env, value, &type, &length, &raw,
                                       &array_buffer, &byte_offset),
              "read typed array") ||
      type != napi_uint8_array) {
    std::string message(field);
    message.append(" must be a Uint8Array");
    napi_throw_type_error(env, "ERR_INVALID_ARG_TYPE", message.c_str());
    return false;
  }
  const auto *bytes = static_cast<const uint8_t *>(raw);
  if (length == 0)
    output->clear();
  else
    output->assign(bytes, bytes + length);
  return true;
}

bool GetRequiredUint32(napi_env env, napi_value object, const char *name,
                       uint32_t *output) {
  napi_value value;
  bool present = false;
  if (!GetNamed(env, object, name, &value, &present))
    return false;
  if (!present) {
    std::string message(name);
    message.append(" is required");
    napi_throw_type_error(env, "ERR_MISSING_ARGS", message.c_str());
    return false;
  }
  return NapiOk(env, napi_get_value_uint32(env, value, output),
                "read unsigned integer");
}

bool ParseAttributeValue(napi_env env, napi_value value,
                         ZeAttributeValue *attribute,
                         std::string *string_storage) {
  *attribute = ZeAttributeValue{};
  if (!GetRequiredUint32(env, value, "id", &attribute->attribute_id))
    return false;
  napi_value js_type;
  bool present = false;
  if (!GetNamed(env, value, "type", &js_type, &present) || !present) {
    napi_throw_type_error(env, "ERR_MISSING_ARGS",
                          "attribute value type is required");
    return false;
  }
  std::string type;
  if (!GetUtf8(env, js_type, "attribute value type", &type))
    return false;
  const char *types[] = {"null", "u64", "i64", "f64", "bool", "string"};
  if (!ParseEnum(type, types, 6, &attribute->value_type)) {
    napi_throw_range_error(env, "ERR_OUT_OF_RANGE",
                           "attribute value type is out of range");
    return false;
  }
  napi_value js_value;
  if (!GetNamed(env, value, "value", &js_value, &present) || !present) {
    napi_throw_type_error(env, "ERR_MISSING_ARGS",
                          "attribute value is required");
    return false;
  }
  bool lossless = false;
  switch (attribute->value_type) {
  case 0: {
    napi_value null_value;
    bool equal = false;
    if (!NapiOk(env, napi_get_null(env, &null_value), "create null") ||
        !NapiOk(env, napi_strict_equals(env, js_value, null_value, &equal),
                "inspect null") ||
        !equal) {
      napi_throw_type_error(env, "ERR_INVALID_ARG_TYPE",
                            "null attribute value must be null");
      return false;
    }
    break;
  }
  case 1:
    if (!NapiOk(env,
                napi_get_value_bigint_uint64(env, js_value,
                                             &attribute->u64_value, &lossless),
                "read u64 attribute") ||
        !lossless) {
      napi_throw_range_error(env, "ERR_OUT_OF_RANGE",
                             "u64 attribute value must be an unsigned bigint");
      return false;
    }
    break;
  case 2:
    if (!NapiOk(env,
                napi_get_value_bigint_int64(env, js_value,
                                            &attribute->i64_value, &lossless),
                "read i64 attribute") ||
        !lossless) {
      napi_throw_range_error(env, "ERR_OUT_OF_RANGE",
                             "i64 attribute value must be a signed bigint");
      return false;
    }
    break;
  case 3:
    if (!NapiOk(env,
                napi_get_value_double(env, js_value, &attribute->f64_value),
                "read f64 attribute")) {
      return false;
    }
    break;
  case 4: {
    bool boolean = false;
    if (!NapiOk(env, napi_get_value_bool(env, js_value, &boolean),
                "read bool attribute")) {
      return false;
    }
    attribute->bool_value = boolean ? 1 : 0;
    break;
  }
  case 5:
    if (!GetUtf8(env, js_value, "string attribute value", string_storage))
      return false;
    attribute->string_value =
        reinterpret_cast<const uint8_t *>(string_storage->data());
    attribute->string_len = string_storage->size();
    break;
  default:
    napi_throw_range_error(env, "ERR_OUT_OF_RANGE",
                           "attribute value type is out of range");
    return false;
  }
  return true;
}

bool ParseDocumentFields(napi_env env, napi_value value, bool default_all,
                         uint32_t *vector, uint32_t *text, uint32_t *metadata,
                         uint32_t *attributes) {
  napi_valuetype type = napi_undefined;
  if (value != nullptr &&
      !NapiOk(env, napi_typeof(env, value, &type), "inspect document fields"))
    return false;
  if (value == nullptr || type == napi_undefined) {
    const uint32_t flag = default_all ? 1 : 0;
    *vector = flag;
    *text = flag;
    *metadata = flag;
    *attributes = flag;
    return true;
  }
  if (type != napi_object) {
    napi_throw_type_error(env, "ERR_INVALID_ARG_TYPE",
                          "fields must be an object");
    return false;
  }
  bool include_vector = false;
  bool include_text = false;
  bool include_metadata = false;
  bool include_attributes = false;
  if (!GetOptionalBool(env, value, "vector", false, &include_vector) ||
      !GetOptionalBool(env, value, "text", false, &include_text) ||
      !GetOptionalBool(env, value, "metadata", false, &include_metadata) ||
      !GetOptionalBool(env, value, "attributes", false, &include_attributes)) {
    return false;
  }
  *vector = include_vector ? 1 : 0;
  *text = include_text ? 1 : 0;
  *metadata = include_metadata ? 1 : 0;
  *attributes = include_attributes ? 1 : 0;
  return true;
}

bool CreateByteArray(napi_env env, const uint8_t *data, size_t length,
                     napi_value *output) {
  void *copy = nullptr;
  napi_value array_buffer;
  if (!NapiOk(env, napi_create_arraybuffer(env, length, &copy, &array_buffer),
              "create byte array buffer")) {
    return false;
  }
  if (length != 0)
    std::memcpy(copy, data, length);
  return NapiOk(env,
                napi_create_typedarray(env, napi_uint8_array, length,
                                       array_buffer, 0, output),
                "create byte array");
}

bool CreateFloatArray(napi_env env, const float *data, size_t length,
                      napi_value *output) {
  void *copy = nullptr;
  napi_value array_buffer;
  if (!NapiOk(env,
              napi_create_arraybuffer(env, length * sizeof(float), &copy,
                                      &array_buffer),
              "create vector array buffer")) {
    return false;
  }
  if (length != 0)
    std::memcpy(copy, data, length * sizeof(float));
  return NapiOk(env,
                napi_create_typedarray(env, napi_float32_array, length,
                                       array_buffer, 0, output),
                "create vector array");
}

bool CreateAttributeValue(napi_env env, const ZeAttributeValue &native,
                          napi_value *output) {
  napi_value result;
  napi_value id;
  napi_value type;
  napi_value value;
  const char *type_name = nullptr;
  if (!NapiOk(env, napi_create_object(env, &result),
              "create attribute value") ||
      !NapiOk(env, napi_create_uint32(env, native.attribute_id, &id),
              "create attribute id") ||
      !SetNamed(env, result, "id", id)) {
    return false;
  }
  switch (native.value_type) {
  case 0:
    type_name = "null";
    if (!NapiOk(env, napi_get_null(env, &value), "create null attribute"))
      return false;
    break;
  case 1:
    type_name = "u64";
    if (!NapiOk(env, napi_create_bigint_uint64(env, native.u64_value, &value),
                "create u64 attribute"))
      return false;
    break;
  case 2:
    type_name = "i64";
    if (!NapiOk(env, napi_create_bigint_int64(env, native.i64_value, &value),
                "create i64 attribute"))
      return false;
    break;
  case 3:
    type_name = "f64";
    if (!NapiOk(env, napi_create_double(env, native.f64_value, &value),
                "create f64 attribute"))
      return false;
    break;
  case 4:
    type_name = "bool";
    if (!NapiOk(env, napi_get_boolean(env, native.bool_value != 0, &value),
                "create bool attribute"))
      return false;
    break;
  case 5:
    type_name = "string";
    {
      const char *string_value =
          native.string_value == nullptr
              ? ""
              : reinterpret_cast<const char *>(native.string_value);
      if (!NapiOk(env,
                  napi_create_string_utf8(env, string_value, native.string_len,
                                          &value),
                  "create string attribute"))
        return false;
    }
    break;
  default:
    napi_throw_error(env, "ERR_ZEPPELIN_NATIVE",
                     "stored attribute has an unknown type");
    return false;
  }
  if (!NapiOk(env,
              napi_create_string_utf8(env, type_name, NAPI_AUTO_LENGTH, &type),
              "create attribute type") ||
      !SetNamed(env, result, "type", type) ||
      !SetNamed(env, result, "value", value)) {
    return false;
  }
  *output = result;
  return true;
}

bool CreateStoredDocument(napi_env env, const ZeStoredDocument &native,
                          napi_value *output) {
  if (native.has_document == 0)
    return NapiOk(env, napi_get_null(env, output), "create missing document");
  napi_value document;
  napi_value id;
  napi_value revision;
  napi_value timestamp;
  if (!NapiOk(env, napi_create_object(env, &document),
              "create stored document") ||
      !CreateUint128(env, native.doc_id, &id) ||
      !SetNamed(env, document, "id", id) ||
      !NapiOk(env, napi_create_bigint_uint64(env, native.revision, &revision),
              "create document revision") ||
      !SetNamed(env, document, "revision", revision) ||
      !NapiOk(env, napi_create_bigint_int64(env, native.timestamp, &timestamp),
              "create document timestamp") ||
      !SetNamed(env, document, "timestamp", timestamp)) {
    return false;
  }
  if (native.vector != nullptr) {
    napi_value vector;
    if (!CreateFloatArray(env, native.vector, native.vector_len, &vector) ||
        !SetNamed(env, document, "vector", vector))
      return false;
  }
  if (native.text != nullptr) {
    napi_value text;
    if (!NapiOk(env,
                napi_create_string_utf8(
                    env, reinterpret_cast<const char *>(native.text),
                    native.text_len, &text),
                "create stored text") ||
        !SetNamed(env, document, "text", text))
      return false;
  }
  if (native.metadata != nullptr) {
    napi_value metadata;
    if (!CreateByteArray(env, native.metadata, native.metadata_len,
                         &metadata) ||
        !SetNamed(env, document, "metadata", metadata))
      return false;
  }
  if (native.attributes != nullptr) {
    napi_value attributes;
    if (!NapiOk(env,
                napi_create_array_with_length(env, native.attribute_count,
                                              &attributes),
                "create stored attributes"))
      return false;
    for (size_t index = 0; index < native.attribute_count; ++index) {
      napi_value attribute;
      if (!CreateAttributeValue(env, native.attributes[index], &attribute) ||
          !NapiOk(env, napi_set_element(env, attributes, index, attribute),
                  "append stored attribute"))
        return false;
    }
    if (!SetNamed(env, document, "attributes", attributes))
      return false;
  }
  *output = document;
  return true;
}

bool CreateStoredDocuments(napi_env env, const ZeStoredDocument *documents,
                           size_t document_count, napi_value *output) {
  napi_value result;
  if (!NapiOk(env, napi_create_array_with_length(env, document_count, &result),
              "create stored document array"))
    return false;
  for (size_t index = 0; index < document_count; ++index) {
    napi_value document;
    if (!CreateStoredDocument(env, documents[index], &document) ||
        !NapiOk(env, napi_set_element(env, result, index, document),
                "append stored document"))
      return false;
  }
  *output = result;
  return true;
}

struct FilterNodeStorage {
  ZeFilterNode native{};
  std::vector<ZeAttributeValue> values;
  std::vector<std::string> value_strings;
  std::string lower_string;
  std::string upper_string;
};

struct FilterStorage {
  std::vector<FilterNodeStorage> storage;
  std::vector<ZeFilterNode> nodes;
  ZeFilter filter{};
};

bool ParseFilterNode(napi_env env, napi_value value, uint32_t index,
                     uint32_t depth, FilterStorage *filter,
                     std::vector<napi_value> *seen) {
  if (depth > 32) {
    filter->storage[index].native.op = 8;
    filter->storage[index].native.children_start = index;
    filter->storage[index].native.children_count = 1;
    return true;
  }
  for (napi_value prior : *seen) {
    bool equal = false;
    if (!NapiOk(env, napi_strict_equals(env, prior, value, &equal),
                "inspect filter cycle"))
      return false;
    if (equal) {
      filter->storage[index].native.op = 8;
      filter->storage[index].native.children_start = index;
      filter->storage[index].native.children_count = 1;
      return true;
    }
  }
  napi_valuetype type;
  if (!NapiOk(env, napi_typeof(env, value, &type), "inspect filter node"))
    return false;
  if (type != napi_object) {
    filter->storage[index].native.op = 8;
    filter->storage[index].native.children_start = UINT32_MAX;
    filter->storage[index].native.children_count = 1;
    return true;
  }
  seen->push_back(value);
  napi_value field;
  bool present = false;
  if (!GetNamed(env, value, "op", &field, &present) || !present) {
    napi_throw_type_error(env, "ERR_MISSING_ARGS", "filter op is required");
    return false;
  }
  std::string operation;
  if (!GetUtf8(env, field, "filter op", &operation))
    return false;
  const char *operations[] = {"eq",     "notEq",  "in",  "notIn", "range",
                              "exists", "isNull", "and", "or",    "not"};
  int32_t parsed_operation = 0;
  if (!ParseEnum(operation, operations, 10, &parsed_operation)) {
    napi_throw_range_error(env, "ERR_OUT_OF_RANGE",
                           "filter op is out of range");
    return false;
  }
  FilterNodeStorage &node = filter->storage[index];
  node.native.op = parsed_operation + 1;
  if (node.native.op <= 7) {
    if (!GetRequiredUint32(env, value, "attributeId",
                           &node.native.attribute_id))
      return false;
  }

  if (node.native.op >= 1 && node.native.op <= 4) {
    napi_value values;
    if (!GetNamed(env, value, "values", &values, &present) || !present) {
      napi_throw_type_error(env, "ERR_MISSING_ARGS",
                            "filter values are required");
      return false;
    }
    bool is_array = false;
    uint32_t value_count = 0;
    if (!NapiOk(env, napi_is_array(env, values, &is_array),
                "inspect filter values") ||
        !is_array) {
      napi_throw_type_error(env, "ERR_INVALID_ARG_TYPE",
                            "filter values must be an array");
      return false;
    }
    if (!NapiOk(env, napi_get_array_length(env, values, &value_count),
                "read filter value count"))
      return false;
    node.values.resize(value_count);
    node.value_strings.resize(value_count);
    for (uint32_t value_index = 0; value_index < value_count; ++value_index) {
      napi_value item;
      if (!NapiOk(env, napi_get_element(env, values, value_index, &item),
                  "read filter value") ||
          !ParseAttributeValue(env, item, &node.values[value_index],
                               &node.value_strings[value_index]))
        return false;
    }
  } else if (node.native.op == 5) {
    napi_value bound;
    if (!GetNamed(env, value, "lower", &bound, &present))
      return false;
    if (present) {
      node.native.has_lower = 1;
      if (!ParseAttributeValue(env, bound, &node.native.lower,
                               &node.lower_string))
        return false;
      bool inclusive = true;
      if (!GetOptionalBool(env, value, "lowerInclusive", true, &inclusive))
        return false;
      node.native.lower_inclusive = inclusive ? 1 : 0;
    }
    if (!GetNamed(env, value, "upper", &bound, &present))
      return false;
    if (present) {
      node.native.has_upper = 1;
      if (!ParseAttributeValue(env, bound, &node.native.upper,
                               &node.upper_string))
        return false;
      bool inclusive = true;
      if (!GetOptionalBool(env, value, "upperInclusive", true, &inclusive))
        return false;
      node.native.upper_inclusive = inclusive ? 1 : 0;
    }
  } else if (node.native.op >= 8) {
    napi_value children;
    if (!GetNamed(env, value, "children", &children, &present) || !present) {
      napi_throw_type_error(env, "ERR_MISSING_ARGS",
                            "logical filter children are required");
      return false;
    }
    bool is_array = false;
    uint32_t child_count = 0;
    if (!NapiOk(env, napi_is_array(env, children, &is_array),
                "inspect filter children") ||
        !is_array) {
      napi_throw_type_error(env, "ERR_INVALID_ARG_TYPE",
                            "filter children must be an array");
      return false;
    }
    if (!NapiOk(env, napi_get_array_length(env, children, &child_count),
                "read filter child count"))
      return false;
    const size_t child_start = filter->storage.size();
    if (child_start > UINT32_MAX ||
        static_cast<size_t>(child_count) > UINT32_MAX - child_start) {
      napi_throw_range_error(env, "ERR_OUT_OF_RANGE",
                             "filter contains too many nodes");
      return false;
    }
    filter->storage.resize(child_start + child_count);
    filter->storage[index].native.children_start =
        static_cast<uint32_t>(child_start);
    filter->storage[index].native.children_count = child_count;
    for (uint32_t child = 0; child < child_count; ++child) {
      napi_value child_value;
      if (!NapiOk(env, napi_get_element(env, children, child, &child_value),
                  "read filter child") ||
          !ParseFilterNode(env, child_value,
                           static_cast<uint32_t>(child_start + child),
                           depth + 1, filter, seen))
        return false;
    }
  }
  return true;
}

bool ParseFilter(napi_env env, napi_value value, FilterStorage *filter) {
  filter->storage.resize(1);
  std::vector<napi_value> seen;
  if (!ParseFilterNode(env, value, 0, 0, filter, &seen))
    return false;
  filter->nodes.resize(filter->storage.size());
  for (size_t index = 0; index < filter->storage.size(); ++index) {
    FilterNodeStorage &stored = filter->storage[index];
    for (size_t value_index = 0; value_index < stored.values.size();
         ++value_index) {
      if (stored.values[value_index].value_type == 5) {
        stored.values[value_index].string_value =
            reinterpret_cast<const uint8_t *>(
                stored.value_strings[value_index].data());
        stored.values[value_index].string_len =
            stored.value_strings[value_index].size();
      }
    }
    ZeFilterNode native = stored.native;
    native.values = stored.values.empty() ? nullptr : stored.values.data();
    native.value_count = stored.values.size();
    if (native.has_lower != 0 && native.lower.value_type == 5) {
      native.lower.string_value =
          reinterpret_cast<const uint8_t *>(stored.lower_string.data());
      native.lower.string_len = stored.lower_string.size();
    }
    if (native.has_upper != 0 && native.upper.value_type == 5) {
      native.upper.string_value =
          reinterpret_cast<const uint8_t *>(stored.upper_string.data());
      native.upper.string_len = stored.upper_string.size();
    }
    filter->nodes[index] = native;
  }
  filter->filter = ZeFilter{};
  filter->filter.abi_size = sizeof(filter->filter);
  filter->filter.nodes = filter->nodes.data();
  filter->filter.node_count = filter->nodes.size();
  return true;
}

bool ParseTimestampRange(napi_env env, napi_value request,
                         uint32_t *has_timestamp_range, int64_t *start_ts,
                         int64_t *end_ts) {
  napi_value range;
  bool present = false;
  if (!GetNamed(env, request, "timestampRange", &range, &present))
    return false;
  if (!present)
    return true;
  napi_valuetype type;
  if (!NapiOk(env, napi_typeof(env, range, &type), "inspect timestamp range") ||
      type != napi_object) {
    napi_throw_type_error(env, "ERR_INVALID_ARG_TYPE",
                          "timestampRange must be an object");
    return false;
  }
  napi_value bound;
  if (!GetNamed(env, range, "start", &bound, &present) || !present ||
      !GetOptionalBigInt64(env, range, "start", 0, start_ts) ||
      !GetNamed(env, range, "end", &bound, &present) || !present ||
      !GetOptionalBigInt64(env, range, "end", 0, end_ts)) {
    if (!present)
      napi_throw_type_error(env, "ERR_MISSING_ARGS",
                            "timestampRange requires start and end");
    return false;
  }
  *has_timestamp_range = 1;
  return true;
}

bool ParseOpenRequest(napi_env env, napi_value options, const std::string &path,
                      ZeOpenRequest *request) {
  napi_valuetype type;
  if (!NapiOk(env, napi_typeof(env, options, &type), "inspect options") ||
      type != napi_object) {
    napi_throw_type_error(env, "ERR_INVALID_ARG_TYPE",
                          "options must be an object");
    return false;
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
    return false;
  }

  int32_t durability = 0;
  int32_t commit_tier = 0;
  std::string value;
  bool present = false;
  if (!GetOptionalString(env, options, "durability", &value, &present))
    return false;
  if (present) {
    const char *names[] = {"derived", "durable", "attached"};
    if (!ParseEnum(value, names, 3, &durability)) {
      napi_throw_range_error(
          env, "ERR_OUT_OF_RANGE",
          "durability must be derived, durable, or attached");
      return false;
    }
  }
  value.clear();
  present = false;
  if (!GetOptionalString(env, options, "commitTier", &value, &present))
    return false;
  if (present) {
    const char *names[] = {"none", "ordered", "durable"};
    if (!ParseEnum(value, names, 3, &commit_tier)) {
      napi_throw_range_error(env, "ERR_OUT_OF_RANGE",
                             "commitTier must be none, ordered, or durable");
      return false;
    }
  }

  *request = ZeOpenRequest{};
  request->abi_size = sizeof(*request);
  request->path = reinterpret_cast<const uint8_t *>(path.data());
  request->path_len = path.size();
  request->access_mode = read_only ? 1 : 0;
  request->durability_mode = durability;
  request->commit_tier = commit_tier;
  request->reader_drain_timeout_ms = drain_ms;
  request->max_resident_bytes = resident_bytes;
  request->max_temp_bytes = temp_bytes;
  return true;
}

bool ParseNamespaceSpec(napi_env env, napi_value value, ZeNamespaceSpec *spec,
                        std::vector<ZeAttributeDefinition> *attributes,
                        std::vector<std::string> *attribute_names) {
  napi_valuetype type;
  if (!NapiOk(env, napi_typeof(env, value, &type), "inspect namespace spec") ||
      type != napi_object) {
    napi_throw_type_error(env, "ERR_INVALID_ARG_TYPE",
                          "spec must be an object");
    return false;
  }

  napi_value js_attributes;
  bool present = false;
  if (!GetNamed(env, value, "attributes", &js_attributes, &present))
    return false;
  uint32_t attribute_count = 0;
  if (present) {
    bool is_array = false;
    if (!NapiOk(env, napi_is_array(env, js_attributes, &is_array),
                "inspect namespace attributes") ||
        !is_array) {
      napi_throw_type_error(env, "ERR_INVALID_ARG_TYPE",
                            "spec.attributes must be an array");
      return false;
    }
    if (!NapiOk(env,
                napi_get_array_length(env, js_attributes, &attribute_count),
                "read namespace attribute count")) {
      return false;
    }
  }
  attributes->resize(attribute_count);
  attribute_names->resize(attribute_count);
  for (uint32_t index = 0; index < attribute_count; ++index) {
    napi_value attribute;
    napi_value field;
    if (!NapiOk(env, napi_get_element(env, js_attributes, index, &attribute),
                "read namespace attribute") ||
        !GetNamed(env, attribute, "id", &field, &present) || !present) {
      napi_throw_type_error(env, "ERR_MISSING_ARGS",
                            "each attribute requires id");
      return false;
    }
    ZeAttributeDefinition &native = (*attributes)[index];
    native = ZeAttributeDefinition{};
    if (!NapiOk(env, napi_get_value_uint32(env, field, &native.attribute_id),
                "read attribute id") ||
        !GetNamed(env, attribute, "name", &field, &present) || !present ||
        !GetUtf8(env, field, "attribute name", &(*attribute_names)[index]) ||
        !GetNamed(env, attribute, "type", &field, &present) || !present) {
      if (!present)
        napi_throw_type_error(env, "ERR_MISSING_ARGS",
                              "each attribute requires name and type");
      return false;
    }
    std::string attribute_type;
    if (!GetUtf8(env, field, "attribute type", &attribute_type))
      return false;
    const char *names[] = {"u64",      "i64", "f64", "bool", "dictionaryString",
                           "rawString"};
    int32_t parsed_type = 0;
    if (!ParseEnum(attribute_type, names, 6, &parsed_type)) {
      napi_throw_range_error(env, "ERR_OUT_OF_RANGE",
                             "attribute type is out of range");
      return false;
    }
    bool nullable = false;
    if (!GetOptionalBool(env, attribute, "nullable", false, &nullable))
      return false;
    native.name =
        reinterpret_cast<const uint8_t *>((*attribute_names)[index].data());
    native.name_len = (*attribute_names)[index].size();
    native.attribute_type = parsed_type + 1;
    native.nullable = nullable ? 1 : 0;
  }

  *spec = ZeNamespaceSpec{};
  spec->abi_size = sizeof(*spec);
  spec->attributes = attributes->data();
  spec->attribute_count = attributes->size();
  napi_value vector_space;
  present = false;
  if (!GetNamed(env, value, "vectorSpace", &vector_space, &present))
    return false;
  if (!present)
    return true;
  if (!NapiOk(env, napi_typeof(env, vector_space, &type),
              "inspect vector space") ||
      type != napi_object) {
    napi_throw_type_error(env, "ERR_INVALID_ARG_TYPE",
                          "spec.vectorSpace must be an object");
    return false;
  }
  napi_value dimensions;
  if (!GetNamed(env, vector_space, "dimensions", &dimensions, &present) ||
      !present) {
    napi_throw_type_error(env, "ERR_MISSING_ARGS",
                          "vectorSpace.dimensions is required");
    return false;
  }
  if (!NapiOk(env, napi_get_value_uint32(env, dimensions, &spec->dimensions),
              "read vector dimensions")) {
    return false;
  }
  spec->has_vector_space = 1;
  std::string normalization;
  if (!GetOptionalString(env, vector_space, "normalization", &normalization,
                         &present)) {
    return false;
  }
  if (present) {
    const char *names[] = {"none", "unitL2"};
    if (!ParseEnum(normalization, names, 2, &spec->normalization)) {
      napi_throw_range_error(env, "ERR_OUT_OF_RANGE",
                             "normalization must be none or unitL2");
      return false;
    }
  }
  return true;
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
    size_t argc = 4;
    napi_value args[4];
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

    ZeOpenRequest request{};
    if (!ParseOpenRequest(env, options, path, &request))
      return nullptr;

    ze_handle handle = 0;
    ze_error_code status = ZE_OK;
    if (argc < 4) {
      status = ze_open(&request, &handle);
    } else {
      std::string name;
      if (!GetUtf8(env, args[2], "name", &name))
        return nullptr;
      ZeNamespaceSpec spec{};
      std::vector<ZeAttributeDefinition> attributes;
      std::vector<std::string> attribute_names;
      if (!ParseNamespaceSpec(env, args[3], &spec, &attributes,
                              &attribute_names)) {
        return nullptr;
      }
      ZeNamespaceOpenRequest namespace_request{};
      namespace_request.abi_size = sizeof(namespace_request);
      namespace_request.root = reinterpret_cast<const uint8_t *>(path.data());
      namespace_request.root_len = path.size();
      namespace_request.name = reinterpret_cast<const uint8_t *>(name.data());
      namespace_request.name_len = name.size();
      namespace_request.open = request;
      namespace_request.spec = &spec;
      status = ze_namespace_open(&namespace_request, &handle);
    }
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

napi_value ListNamespaces(napi_env env, napi_callback_info info) {
  return Guard(env, [&]() -> napi_value {
    size_t argc = 1;
    napi_value args[1];
    if (!NapiOk(env, napi_get_cb_info(env, info, &argc, args, nullptr, nullptr),
                "read namespace list arguments")) {
      return nullptr;
    }
    if (argc < 1) {
      napi_throw_type_error(env, "ERR_MISSING_ARGS", "root is required");
      return nullptr;
    }
    std::string root;
    if (!GetUtf8(env, args[0], "root", &root))
      return nullptr;
    ZeNamespaceListRequest request{};
    request.abi_size = sizeof(request);
    request.root = reinterpret_cast<const uint8_t *>(root.data());
    request.root_len = root.size();
    ZeNamespaceListResult result{};
    result.abi_size = sizeof(result);
    const ze_error_code status = ze_namespace_list(&request, &result);
    if (status != ZE_OK)
      return ThrowZeppelin(env, 0, status);
    ResultOwner<ZeNamespaceListResult, ze_namespace_list_result_free> owner(
        &result);

    napi_value names;
    if (!NapiOk(env,
                napi_create_array_with_length(env, result.entry_count, &names),
                "create namespace array")) {
      return nullptr;
    }
    for (size_t index = 0; index < result.entry_count; ++index) {
      napi_value name;
      const ZeNamespaceEntry &entry = result.entries[index];
      if (!NapiOk(env,
                  napi_create_string_utf8(
                      env, reinterpret_cast<const char *>(entry.name),
                      entry.name_len, &name),
                  "create namespace name") ||
          !NapiOk(env, napi_set_element(env, names, index, name),
                  "append namespace name")) {
        return nullptr;
      }
    }
    const ze_error_code free_status = owner.FreeNow();
    if (free_status != ZE_OK)
      return ThrowZeppelin(env, 0, free_status);
    return names;
  });
}

napi_value Upsert(napi_env env, napi_callback_info info) {
  return Guard(env, [&]() -> napi_value {
    size_t argc = 1;
    napi_value args[1];
    napi_value receiver;
    if (!NapiOk(env,
                napi_get_cb_info(env, info, &argc, args, &receiver, nullptr),
                "read upsert arguments")) {
      return nullptr;
    }
    if (argc < 1) {
      napi_throw_type_error(env, "ERR_MISSING_ARGS", "documents are required");
      return nullptr;
    }
    NativeStore *store = UnwrapStore(env, receiver);
    if (store == nullptr)
      return nullptr;
    bool is_array = false;
    uint32_t document_count = 0;
    if (!NapiOk(env, napi_is_array(env, args[0], &is_array),
                "inspect upsert documents") ||
        !is_array) {
      napi_throw_type_error(env, "ERR_INVALID_ARG_TYPE",
                            "documents must be an array");
      return nullptr;
    }
    if (!NapiOk(env, napi_get_array_length(env, args[0], &document_count),
                "read upsert document count"))
      return nullptr;

    std::vector<ZeUpsertDocument> documents(document_count);
    std::vector<std::vector<float>> vectors(document_count);
    std::vector<std::vector<uint8_t>> metadata(document_count);
    std::vector<std::string> texts(document_count);
    std::vector<std::vector<ZeAttributeValue>> attributes(document_count);
    std::vector<std::vector<std::string>> attribute_strings(document_count);
    size_t dimension = 0;
    for (uint32_t index = 0; index < document_count; ++index) {
      napi_value document;
      if (!NapiOk(env, napi_get_element(env, args[0], index, &document),
                  "read upsert document"))
        return nullptr;
      napi_value field;
      bool present = false;
      ZeUpsertDocument &native = documents[index];
      native = ZeUpsertDocument{};
      native.abi_size = sizeof(native);
      native.document.abi_size = sizeof(native.document);
      if (!GetNamed(env, document, "id", &field, &present) || !present) {
        napi_throw_type_error(env, "ERR_MISSING_ARGS",
                              "each document requires id");
        return nullptr;
      }
      if (!GetDocId(env, field, &native.document.doc_id) ||
          !GetOptionalBigUint64(env, document, "revision", 1,
                                &native.document.revision) ||
          !GetOptionalBigInt64(env, document, "timestamp", 0,
                               &native.document.timestamp)) {
        return nullptr;
      }

      if (!GetNamed(env, document, "vector", &field, &present))
        return nullptr;
      if (present) {
        const float *data = nullptr;
        size_t length = 0;
        if (!GetFloat32Array(env, field, "document vector", &data, &length))
          return nullptr;
        if (length != 0)
          vectors[index].assign(data, data + length);
        native.document.vector = vectors[index].data();
        native.document.vector_len = vectors[index].size();
        if (dimension == 0)
          dimension = length;
      }

      if (!GetNamed(env, document, "metadata", &field, &present))
        return nullptr;
      if (present) {
        if (!GetUint8ArrayCopy(env, field, "document metadata",
                               &metadata[index]))
          return nullptr;
        native.document.metadata = metadata[index].data();
        native.document.metadata_len = metadata[index].size();
      }

      if (!GetNamed(env, document, "text", &field, &present))
        return nullptr;
      if (present) {
        if (!GetUtf8(env, field, "document text", &texts[index]))
          return nullptr;
        native.document.text =
            reinterpret_cast<const uint8_t *>(texts[index].data());
        native.document.text_len = texts[index].size();
      }

      napi_value js_attributes;
      if (!GetNamed(env, document, "attributes", &js_attributes, &present))
        return nullptr;
      uint32_t attribute_count = 0;
      if (present) {
        if (!NapiOk(env, napi_is_array(env, js_attributes, &is_array),
                    "inspect document attributes") ||
            !is_array) {
          napi_throw_type_error(env, "ERR_INVALID_ARG_TYPE",
                                "document attributes must be an array");
          return nullptr;
        }
        if (!NapiOk(env,
                    napi_get_array_length(env, js_attributes, &attribute_count),
                    "read document attribute count"))
          return nullptr;
      }
      attributes[index].resize(attribute_count);
      attribute_strings[index].resize(attribute_count);
      for (uint32_t attribute_index = 0; attribute_index < attribute_count;
           ++attribute_index) {
        napi_value attribute;
        if (!NapiOk(env,
                    napi_get_element(env, js_attributes, attribute_index,
                                     &attribute),
                    "read document attribute") ||
            !ParseAttributeValue(env, attribute,
                                 &attributes[index][attribute_index],
                                 &attribute_strings[index][attribute_index])) {
          return nullptr;
        }
      }
      native.attributes =
          attributes[index].empty() ? nullptr : attributes[index].data();
      native.attribute_count = attributes[index].size();
    }

    ZeUpsertRequest request{};
    request.abi_size = sizeof(request);
    request.documents = documents.data();
    request.document_count = documents.size();
    request.dimension = dimension;
    ZeMutationReport report{};
    report.abi_size = sizeof(report);
    const ze_error_code status = ze_upsert(store->handle, &request, &report);
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

napi_value Get(napi_env env, napi_callback_info info) {
  return Guard(env, [&]() -> napi_value {
    size_t argc = 2;
    napi_value args[2];
    napi_value receiver;
    if (!NapiOk(env,
                napi_get_cb_info(env, info, &argc, args, &receiver, nullptr),
                "read get arguments")) {
      return nullptr;
    }
    if (argc < 1) {
      napi_throw_type_error(env, "ERR_MISSING_ARGS", "ids are required");
      return nullptr;
    }
    NativeStore *store = UnwrapStore(env, receiver);
    if (store == nullptr)
      return nullptr;
    bool is_array = false;
    uint32_t id_count = 0;
    if (!NapiOk(env, napi_is_array(env, args[0], &is_array), "inspect ids") ||
        !is_array) {
      napi_throw_type_error(env, "ERR_INVALID_ARG_TYPE",
                            "ids must be an array");
      return nullptr;
    }
    if (!NapiOk(env, napi_get_array_length(env, args[0], &id_count),
                "read id count"))
      return nullptr;
    std::vector<ZeDocId> ids(id_count);
    for (uint32_t index = 0; index < id_count; ++index) {
      napi_value id;
      if (!NapiOk(env, napi_get_element(env, args[0], index, &id),
                  "read document id") ||
          !GetDocId(env, id, &ids[index]))
        return nullptr;
    }

    ZeGetRequest request{};
    request.abi_size = sizeof(request);
    request.ids = ids.data();
    request.id_count = ids.size();
    if (!ParseDocumentFields(env, argc < 2 ? nullptr : args[1], true,
                             &request.include_vector, &request.include_text,
                             &request.include_metadata,
                             &request.include_attributes)) {
      return nullptr;
    }
    ZeGetResult native{};
    native.abi_size = sizeof(native);
    const ze_error_code status = ze_get(store->handle, &request, &native);
    if (status != ZE_OK)
      return ThrowZeppelin(env, store->handle, status);
    ResultOwner<ZeGetResult, ze_get_result_free> owner(&native);

    napi_value result;
    napi_value documents;
    napi_value missing_count;
    napi_value generation;
    if (!NapiOk(env, napi_create_object(env, &result), "create get result") ||
        !CreateStoredDocuments(env, native.documents, native.document_count,
                               &documents) ||
        !SetNamed(env, result, "documents", documents) ||
        !NapiOk(env,
                napi_create_double(env,
                                   static_cast<double>(native.missing_count),
                                   &missing_count),
                "create missing count") ||
        !SetNamed(env, result, "missingCount", missing_count) ||
        !NapiOk(env,
                napi_create_bigint_uint64(env, native.generation, &generation),
                "create get generation") ||
        !SetNamed(env, result, "generation", generation)) {
      return nullptr;
    }
    const ze_error_code free_status = owner.FreeNow();
    if (free_status != ZE_OK)
      return ThrowZeppelin(env, store->handle, free_status);
    return result;
  });
}

napi_value DeleteDocuments(napi_env env, napi_callback_info info) {
  return Guard(env, [&]() -> napi_value {
    size_t argc = 1;
    napi_value args[1];
    napi_value receiver;
    if (!NapiOk(env,
                napi_get_cb_info(env, info, &argc, args, &receiver, nullptr),
                "read delete arguments"))
      return nullptr;
    if (argc < 1) {
      napi_throw_type_error(env, "ERR_MISSING_ARGS", "ids are required");
      return nullptr;
    }
    NativeStore *store = UnwrapStore(env, receiver);
    if (store == nullptr)
      return nullptr;
    bool is_array = false;
    uint32_t id_count = 0;
    if (!NapiOk(env, napi_is_array(env, args[0], &is_array), "inspect ids") ||
        !is_array) {
      napi_throw_type_error(env, "ERR_INVALID_ARG_TYPE",
                            "ids must be an array");
      return nullptr;
    }
    if (!NapiOk(env, napi_get_array_length(env, args[0], &id_count),
                "read id count"))
      return nullptr;
    std::vector<ZeDocId> ids(id_count);
    for (uint32_t index = 0; index < id_count; ++index) {
      napi_value id;
      if (!NapiOk(env, napi_get_element(env, args[0], index, &id),
                  "read document id") ||
          !GetDocId(env, id, &ids[index]))
        return nullptr;
    }
    ZeDeleteRequest request{};
    request.abi_size = sizeof(request);
    request.doc_ids = ids.data();
    request.doc_id_count = ids.size();
    ZeMutationReport report{};
    report.abi_size = sizeof(report);
    const ze_error_code status = ze_delete(store->handle, &request, &report);
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
        !SetNamed(env, result, "generation", generation))
      return nullptr;
    return result;
  });
}

napi_value Scan(napi_env env, napi_callback_info info) {
  return Guard(env, [&]() -> napi_value {
    size_t argc = 1;
    napi_value args[1];
    napi_value receiver;
    if (!NapiOk(env,
                napi_get_cb_info(env, info, &argc, args, &receiver, nullptr),
                "read scan arguments"))
      return nullptr;
    NativeStore *store = UnwrapStore(env, receiver);
    if (store == nullptr)
      return nullptr;

    napi_value options = argc == 0 ? nullptr : args[0];
    napi_valuetype options_type = napi_undefined;
    if (options != nullptr &&
        !NapiOk(env, napi_typeof(env, options, &options_type),
                "inspect scan request"))
      return nullptr;
    if (options != nullptr && options_type != napi_undefined &&
        options_type != napi_object) {
      napi_throw_type_error(env, "ERR_INVALID_ARG_TYPE",
                            "scan request must be an object");
      return nullptr;
    }
    const bool has_options = options != nullptr && options_type == napi_object;
    ZeScanRequest request{};
    request.abi_size = sizeof(request);
    request.limit = 1000;
    request.include_vector = 1;
    request.include_text = 1;
    request.include_metadata = 1;
    request.include_attributes = 1;
    FilterStorage filter_storage;

    napi_value field;
    bool present = false;
    if (has_options) {
      if (!GetNamed(env, options, "limit", &field, &present))
        return nullptr;
      if (present) {
        uint32_t limit = 0;
        if (!NapiOk(env, napi_get_value_uint32(env, field, &limit),
                    "read scan limit"))
          return nullptr;
        request.limit = limit;
      }

      std::string order;
      if (!GetOptionalString(env, options, "order", &order, &present))
        return nullptr;
      if (present) {
        const char *orders[] = {"storage", "timestampAscending",
                                "timestampDescending"};
        if (!ParseEnum(order, orders, 3, &request.order)) {
          napi_throw_range_error(env, "ERR_OUT_OF_RANGE",
                                 "scan order is out of range");
          return nullptr;
        }
      }

      if (!GetNamed(env, options, "fields", &field, &present))
        return nullptr;
      if (!ParseDocumentFields(env, present ? field : nullptr, true,
                               &request.include_vector, &request.include_text,
                               &request.include_metadata,
                               &request.include_attributes))
        return nullptr;

      if (!GetNamed(env, options, "cursor", &field, &present))
        return nullptr;
      if (present) {
        napi_valuetype cursor_type;
        if (!NapiOk(env, napi_typeof(env, field, &cursor_type),
                    "inspect scan cursor"))
          return nullptr;
        if (cursor_type != napi_undefined) {
          if (cursor_type != napi_object) {
            napi_throw_type_error(env, "ERR_INVALID_ARG_TYPE",
                                  "scan cursor must be an object");
            return nullptr;
          }
          napi_value cursor_field;
          if (!GetNamed(env, field, "generation", &cursor_field, &present) ||
              !present ||
              !GetOptionalUint64(env, field, "generation", 0,
                                 &request.cursor_generation) ||
              !GetNamed(env, field, "segmentId", &cursor_field, &present) ||
              !present) {
            napi_throw_type_error(env, "ERR_MISSING_ARGS",
                                  "scan cursor is malformed");
            return nullptr;
          }
          std::vector<uint8_t> segment_id;
          if (!GetUint8ArrayCopy(env, cursor_field, "cursor segmentId",
                                 &segment_id))
            return nullptr;
          if (segment_id.size() != sizeof(request.cursor_segment_id)) {
            napi_throw_range_error(env, "ERR_OUT_OF_RANGE",
                                   "cursor segmentId must contain 16 bytes");
            return nullptr;
          }
          std::memcpy(request.cursor_segment_id, segment_id.data(),
                      segment_id.size());
          if (!GetRequiredUint32(env, field, "nextRow",
                                 &request.cursor_next_row) ||
              !GetRequiredUint32(env, field, "phase", &request.cursor_phase))
            return nullptr;
        }
      }
      if (!ParseTimestampRange(env, options, &request.has_timestamp_range,
                               &request.start_ts, &request.end_ts))
        return nullptr;
      if (!GetNamed(env, options, "filter", &field, &present))
        return nullptr;
      if (present) {
        if (!ParseFilter(env, field, &filter_storage))
          return nullptr;
        request.filter = &filter_storage.filter;
      }
    }

    ZeScanResult native{};
    native.abi_size = sizeof(native);
    const ze_error_code status = ze_scan(store->handle, &request, &native);
    if (status != ZE_OK)
      return ThrowZeppelin(env, store->handle, status);
    ResultOwner<ZeScanResult, ze_scan_result_free> owner(&native);
    napi_value result;
    napi_value documents;
    napi_value generation;
    napi_value cursor;
    if (!NapiOk(env, napi_create_object(env, &result), "create scan page") ||
        !CreateStoredDocuments(env, native.documents, native.document_count,
                               &documents) ||
        !SetNamed(env, result, "documents", documents) ||
        !NapiOk(env,
                napi_create_bigint_uint64(env, native.generation, &generation),
                "create scan generation") ||
        !SetNamed(env, result, "generation", generation))
      return nullptr;
    if (native.has_more == 0) {
      if (!NapiOk(env, napi_get_null(env, &cursor), "create empty cursor"))
        return nullptr;
    } else {
      napi_value segment_id;
      napi_value next_row;
      napi_value phase;
      if (!NapiOk(env, napi_create_object(env, &cursor),
                  "create scan cursor") ||
          !SetNamed(env, cursor, "generation", generation) ||
          !CreateByteArray(env, native.next_segment_id,
                           sizeof(native.next_segment_id), &segment_id) ||
          !SetNamed(env, cursor, "segmentId", segment_id) ||
          !NapiOk(env, napi_create_uint32(env, native.next_row, &next_row),
                  "create cursor row") ||
          !SetNamed(env, cursor, "nextRow", next_row) ||
          !NapiOk(env, napi_create_uint32(env, native.next_phase, &phase),
                  "create cursor phase") ||
          !SetNamed(env, cursor, "phase", phase))
        return nullptr;
    }
    if (!SetNamed(env, result, "cursor", cursor))
      return nullptr;
    const ze_error_code free_status = owner.FreeNow();
    if (free_status != ZE_OK)
      return ThrowZeppelin(env, store->handle, free_status);
    return result;
  });
}

napi_value Count(napi_env env, napi_callback_info info) {
  return Guard(env, [&]() -> napi_value {
    size_t argc = 1;
    napi_value args[1];
    napi_value receiver;
    if (!NapiOk(env,
                napi_get_cb_info(env, info, &argc, args, &receiver, nullptr),
                "read count arguments"))
      return nullptr;
    NativeStore *store = UnwrapStore(env, receiver);
    if (store == nullptr)
      return nullptr;
    napi_value options = argc == 0 ? nullptr : args[0];
    napi_valuetype options_type = napi_undefined;
    if (options != nullptr &&
        !NapiOk(env, napi_typeof(env, options, &options_type),
                "inspect count request"))
      return nullptr;
    if (options != nullptr && options_type != napi_undefined &&
        options_type != napi_object) {
      napi_throw_type_error(env, "ERR_INVALID_ARG_TYPE",
                            "count request must be an object");
      return nullptr;
    }
    ZeCountRequest request{};
    request.abi_size = sizeof(request);
    FilterStorage filter_storage;
    if (options != nullptr && options_type == napi_object) {
      if (!ParseTimestampRange(env, options, &request.has_timestamp_range,
                               &request.start_ts, &request.end_ts))
        return nullptr;
      napi_value filter;
      bool present = false;
      if (!GetNamed(env, options, "filter", &filter, &present))
        return nullptr;
      if (present) {
        if (!ParseFilter(env, filter, &filter_storage))
          return nullptr;
        request.filter = &filter_storage.filter;
      }
    }
    ZeCountResult native{};
    native.abi_size = sizeof(native);
    const ze_error_code status = ze_count(store->handle, &request, &native);
    if (status != ZE_OK)
      return ThrowZeppelin(env, store->handle, status);
    napi_value result;
    napi_value count;
    napi_value generation;
    if (!NapiOk(env, napi_create_object(env, &result), "create count result") ||
        !NapiOk(env, napi_create_bigint_uint64(env, native.count, &count),
                "create document count") ||
        !SetNamed(env, result, "count", count) ||
        !NapiOk(env,
                napi_create_bigint_uint64(env, native.generation, &generation),
                "create count generation") ||
        !SetNamed(env, result, "generation", generation))
      return nullptr;
    return result;
  });
}

napi_value SearchFiltered(napi_env env, napi_callback_info info) {
  return Guard(env, [&]() -> napi_value {
    size_t argc = 3;
    napi_value args[3];
    napi_value receiver;
    if (!NapiOk(env,
                napi_get_cb_info(env, info, &argc, args, &receiver, nullptr),
                "read filtered search arguments"))
      return nullptr;
    if (argc < 2) {
      napi_throw_type_error(env, "ERR_MISSING_ARGS",
                            "vector and filter are required");
      return nullptr;
    }
    NativeStore *store = UnwrapStore(env, receiver);
    if (store == nullptr)
      return nullptr;
    const float *vector_data = nullptr;
    size_t vector_length = 0;
    if (!GetFloat32Array(env, args[0], "query vector", &vector_data,
                         &vector_length))
      return nullptr;
    std::vector<float> vector;
    if (vector_length != 0)
      vector.assign(vector_data, vector_data + vector_length);
    FilterStorage filter;
    if (!ParseFilter(env, args[1], &filter))
      return nullptr;

    ZeSearchFilteredRequest request{};
    request.abi_size = sizeof(request);
    request.search.abi_size = sizeof(request.search);
    request.search.vector = vector.data();
    request.search.vector_len = vector.size();
    request.search.dimension = vector.size();
    request.search.k = 10;
    request.filter = &filter.filter;
    if (argc >= 3) {
      napi_valuetype type;
      if (!NapiOk(env, napi_typeof(env, args[2], &type),
                  "inspect search options"))
        return nullptr;
      if (type != napi_undefined && type != napi_object) {
        napi_throw_type_error(env, "ERR_INVALID_ARG_TYPE",
                              "search options must be an object");
        return nullptr;
      }
      if (type == napi_object) {
        napi_value field;
        bool present = false;
        if (!GetNamed(env, args[2], "k", &field, &present))
          return nullptr;
        if (present) {
          uint32_t k = 0;
          if (!NapiOk(env, napi_get_value_uint32(env, field, &k),
                      "read search k"))
            return nullptr;
          request.search.k = k;
        }
        if (!GetNamed(env, args[2], "threadBudget", &field, &present))
          return nullptr;
        if (present) {
          uint32_t thread_budget = 0;
          if (!NapiOk(env, napi_get_value_uint32(env, field, &thread_budget),
                      "read search thread budget"))
            return nullptr;
          request.search.thread_budget = thread_budget;
        }
        std::string value;
        if (!GetOptionalString(env, args[2], "tier", &value, &present))
          return nullptr;
        if (present) {
          const char *tiers[] = {"auto", "exact", "scan", "graph"};
          if (!ParseEnum(value, tiers, 4, &request.search.tier)) {
            napi_throw_range_error(env, "ERR_OUT_OF_RANGE",
                                   "search tier is out of range");
            return nullptr;
          }
          request.search.has_tier = 1;
        }
        value.clear();
        if (!GetOptionalString(env, args[2], "graphProfile", &value, &present))
          return nullptr;
        if (present) {
          const char *profiles[] = {"sift", "angular"};
          if (!ParseEnum(value, profiles, 2, &request.search.graph_profile)) {
            napi_throw_range_error(env, "ERR_OUT_OF_RANGE",
                                   "graph profile is out of range");
            return nullptr;
          }
        }
        if (!GetNamed(env, args[2], "graphEf", &field, &present))
          return nullptr;
        if (present) {
          uint32_t graph_ef = 0;
          if (!NapiOk(env, napi_get_value_uint32(env, field, &graph_ef),
                      "read graph ef"))
            return nullptr;
          request.search.graph_ef = graph_ef;
        }
        if (!GetOptionalUint64(env, args[2], "graphSeed", 0,
                               &request.search.graph_seed) ||
            !GetOptionalUint64(env, args[2], "deadlineNs", 0,
                               &request.search.deadline_ns))
          return nullptr;
      }
    }

    ZeSearchResult native{};
    native.abi_size = sizeof(native);
    const ze_error_code status =
        ze_search_filtered(store->handle, &request, &native);
    if (status != ZE_OK)
      return ThrowZeppelin(env, store->handle, status);
    ResultOwner<ZeSearchResult, ze_search_result_free> owner(&native);
    napi_value hits;
    if (!NapiOk(env,
                napi_create_array_with_length(env, native.hit_count, &hits),
                "create filtered hit array"))
      return nullptr;
    for (size_t index = 0; index < native.hit_count; ++index) {
      const ZeSearchHit &hit = native.hits[index];
      if (hit.has_document == 0) {
        napi_throw_error(
            env, "ERR_ZEPPELIN_NATIVE",
            "filtered search returned a hit without a document id");
        return nullptr;
      }
      napi_value result;
      napi_value id;
      napi_value revision;
      napi_value score;
      if (!NapiOk(env, napi_create_object(env, &result),
                  "create filtered search hit") ||
          !CreateUint128(env, hit.doc_id, &id) ||
          !SetNamed(env, result, "id", id) ||
          !NapiOk(env, napi_create_bigint_uint64(env, hit.revision, &revision),
                  "create filtered revision") ||
          !SetNamed(env, result, "revision", revision) ||
          !NapiOk(env, napi_create_double(env, hit.score, &score),
                  "create filtered score") ||
          !SetNamed(env, result, "score", score) ||
          !NapiOk(env, napi_set_element(env, hits, index, result),
                  "append filtered search hit"))
        return nullptr;
    }
    const ze_error_code free_status = owner.FreeNow();
    if (free_status != ZE_OK)
      return ThrowZeppelin(env, store->handle, free_status);
    return hits;
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
      {"upsert", nullptr, Upsert, nullptr, nullptr, nullptr, napi_default,
       nullptr},
      {"get", nullptr, Get, nullptr, nullptr, nullptr, napi_default, nullptr},
      {"delete", nullptr, DeleteDocuments, nullptr, nullptr, nullptr,
       napi_default, nullptr},
      {"scan", nullptr, Scan, nullptr, nullptr, nullptr, napi_default, nullptr},
      {"count", nullptr, Count, nullptr, nullptr, nullptr, napi_default,
       nullptr},
      {"searchFiltered", nullptr, SearchFiltered, nullptr, nullptr, nullptr,
       napi_default, nullptr},
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
  napi_value list_namespaces;
  if (!NapiOk(env,
              napi_create_function(env, "listNamespaces", NAPI_AUTO_LENGTH,
                                   ListNamespaces, nullptr, &list_namespaces),
              "create listNamespaces") ||
      !SetNamed(env, exports, "listNamespaces", list_namespaces)) {
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
