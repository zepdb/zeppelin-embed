// Included inside addon.cc's anonymous namespace. Only the arm64 slice links
// graph.
#ifdef ZE_GRAPH
struct GraphPending {};
void GCheck(napi_env env, napi_status status) {
  if (!NapiOk(env, status, "graph binding"))
    throw GraphPending{};
}
template <typename F> napi_value GGuard(napi_env env, F &&fn) {
  return Guard(env, [&]() -> napi_value {
    try {
      return fn();
    } catch (const GraphPending &) {
      return nullptr;
    }
  });
}
template <typename T> T GStruct() {
  T value{};
  value.abi_size = sizeof(T);
  return value;
}
napi_value GObject(napi_env env) {
  napi_value v;
  GCheck(env, napi_create_object(env, &v));
  return v;
}
napi_value GArray(napi_env env, size_t n) {
  napi_value v;
  GCheck(env, napi_create_array_with_length(env, n, &v));
  return v;
}
napi_value GString(napi_env env, const std::string &s) {
  napi_value v;
  GCheck(env, napi_create_string_utf8(env, s.data(), s.size(), &v));
  return v;
}
napi_value GU64(napi_env env, uint64_t n) {
  napi_value v;
  GCheck(env, napi_create_bigint_uint64(env, n, &v));
  return v;
}
napi_value GNumber(napi_env env, double n) {
  napi_value v;
  GCheck(env, napi_create_double(env, n, &v));
  return v;
}
napi_value GBool(napi_env env, bool b) {
  napi_value v;
  GCheck(env, napi_get_boolean(env, b, &v));
  return v;
}
napi_value GNull(napi_env env) {
  napi_value v;
  GCheck(env, napi_get_null(env, &v));
  return v;
}
void GSet(napi_env env, napi_value obj, const char *name, napi_value value) {
  GCheck(env, napi_set_named_property(env, obj, name, value));
}
void GElement(napi_env env, napi_value arr, uint32_t i, napi_value v) {
  GCheck(env, napi_set_element(env, arr, i, v));
}
napi_value GGet(napi_env env, napi_value obj, const char *name) {
  napi_value v;
  GCheck(env, napi_get_named_property(env, obj, name, &v));
  return v;
}
napi_value GAt(napi_env env, napi_value arr, uint32_t i) {
  napi_value v;
  GCheck(env, napi_get_element(env, arr, i, &v));
  return v;
}
napi_valuetype GType(napi_env env, napi_value v) {
  napi_valuetype t;
  GCheck(env, napi_typeof(env, v, &t));
  return t;
}
uint32_t GLength(napi_env env, napi_value arr) {
  uint32_t n;
  GCheck(env, napi_get_array_length(env, arr, &n));
  return n;
}
void GRequire(napi_env env, bool condition, const char *message);
std::string GText(napi_env env, napi_value v) {
  size_t count;
  GCheck(env, napi_get_value_string_utf16(env, v, nullptr, 0, &count));
  std::vector<char16_t> units(count + 1);
  GCheck(env, napi_get_value_string_utf16(env, v, units.data(), units.size(),
                                          &count));
  for (size_t i = 0; i < count; ++i) {
    const auto c = units[i];
    if (c >= 0xd800 && c <= 0xdbff) {
      GRequire(env,
               i + 1 < count && units[i + 1] >= 0xdc00 &&
                   units[i + 1] <= 0xdfff,
               "graph string contains an unpaired surrogate");
      ++i;
    } else
      GRequire(env, c < 0xdc00 || c > 0xdfff,
               "graph string contains an unpaired surrogate");
  }
  std::string s;
  if (!GetUtf8(env, v, "graph string", &s))
    throw GraphPending{};
  return s;
}
void GRequire(napi_env env, bool condition, const char *message) {
  if (condition)
    return;
  napi_value error;
  if (!CreateZeppelinErrorWithMessage(env, ZE_ERR_INVALID_ARGUMENT, message,
                                      &error))
    throw GraphPending{};
  GSet(env, error, "disposition", GString(env, "NotCommitted"));
  GCheck(env, napi_throw(env, error));
  throw GraphPending{};
}
uint64_t GInteger(napi_env env, napi_value v) {
  uint64_t n;
  bool lossless;
  GCheck(env, napi_get_value_bigint_uint64(env, v, &n, &lossless));
  GRequire(env, lossless, "unsigned bigint overflow");
  return n;
}
double GDouble(napi_env env, napi_value v) {
  double n;
  GCheck(env, napi_get_value_double(env, v, &n));
  return n;
}
ZeNodeId GId(napi_env env, napi_value v) {
  uint64_t words[2]{};
  size_t count = 2;
  int sign;
  GCheck(env, napi_get_value_bigint_words(env, v, &sign, &count, words));
  GRequire(env, sign == 0 && count <= 2,
           "entity ID must be an unsigned 128-bit bigint");
  return {words[1], words[0]};
}
napi_value GIdValue(napi_env env, uint64_t high, uint64_t low) {
  const uint64_t words[] = {low, high};
  napi_value v;
  GCheck(env, napi_create_bigint_words(env, 0, 2, words, &v));
  return v;
}
const char *GDisposition(uint32_t value) {
  switch (value) {
  case 0:
    return "NotApplicable";
  case 1:
    return "NotCommitted";
  case 2:
    return "Committed";
  case 3:
    return "Replayed";
  case 4:
    return "NoOp";
  case 5:
    return "Indeterminate";
  default:
    throw std::runtime_error("invalid graph disposition");
  }
}
void GMetadata(napi_env env, napi_value obj, const ZeGraphResponse &r) {
  GSet(env, obj, "disposition", GString(env, GDisposition(r.disposition)));
  GSet(env, obj, "admittedGeneration",
       r.has_admitted_generation ? GU64(env, r.admitted_generation)
                                 : GNull(env));
  GSet(env, obj, "changedGeneration",
       r.has_changed_generation ? GU64(env, r.changed_generation) : GNull(env));
  GSet(env, obj, "generation",
       r.has_changed_generation    ? GU64(env, r.changed_generation)
       : r.has_admitted_generation ? GU64(env, r.admitted_generation)
                                   : GNull(env));
}
ze_handle GHandle(napi_env env, napi_value receiver) {
  auto *store = UnwrapStore(env, receiver);
  if (!store)
    throw GraphPending{};
  return store->handle;
}
ze_error_code CreateStoreWithRelationshipTypes(napi_env env,
                                               const std::string &path,
                                               const ZeOpenRequest &open,
                                               napi_value types,
                                               ze_handle *out) {
  try {
    const auto count = GLength(env, types);
    std::vector<std::string> names;
    names.reserve(count);
    std::vector<ZeGraphRelationshipType> rules;
    rules.reserve(count);
    for (uint32_t i = 0; i < count; ++i) {
      auto js = GAt(env, types, i);
      names.push_back(GText(env, GGet(env, js, "type")));
      auto rule = GStruct<ZeGraphRelationshipType>();
      rule.name = {reinterpret_cast<const uint8_t *>(names.back().data()),
                   names.back().size()};
      const auto action = GText(env, GGet(env, js, "onDelete"));
      rule.on_delete = action == "restrict" ? 1 : 2;
      rules.push_back(rule);
    }
    auto request = GStruct<ZeGraphOpenRequest>();
    request.path = {reinterpret_cast<const uint8_t *>(path.data()),
                    path.size()};
    request.max_resident_bytes = open.max_resident_bytes;
    request.reader_drain_timeout_ms = open.reader_drain_timeout_ms;
    return ze_store_create_with_relationship_types(&request, rules.data(),
                                                   rules.size(), out);
  } catch (const GraphPending &) {
    return ZE_OK;
  }
}
ZeGraphControl GControl(napi_env env, napi_value js) {
  auto c = GStruct<ZeGraphControl>();
  auto token = GGet(env, js, "cancelToken"),
       deadline = GGet(env, js, "deadlineNs");
  if (GType(env, token) != napi_undefined)
    c.cancel_token = GInteger(env, token);
  if (GType(env, deadline) != napi_undefined)
    c.deadline_ns = GInteger(env, deadline);
  GRequire(env, !c.cancel_token || !c.deadline_ns,
           "use one interruption source");
  return c;
}
struct GResponse {
  ZeGraphResponse value = GStruct<ZeGraphResponse>();
  GResponse() { value.pool = GStruct<ZeGraphValuePool>(); }
  ~GResponse() { ze_graph_response_free(&value); }
};
struct GPool {
  std::vector<float> vectors;
  std::vector<uint8_t> bytes;
  std::vector<ZeGraphValue> values;
  std::vector<uint32_t> children;
  std::vector<ZeGraphRange> names;
  std::vector<ZeGraphProperty> properties;
  std::vector<ZeGraphNode> nodes;
  std::vector<ZeGraphRelationship> relationships;
  ZeGraphRange text(const std::string &s) {
    if (s.size() > UINT32_MAX || bytes.size() > UINT32_MAX - s.size())
      throw std::length_error("graph text pool too large");
    ZeGraphRange r{static_cast<uint32_t>(bytes.size()),
                   static_cast<uint32_t>(s.size())};
    bytes.insert(bytes.end(), s.begin(), s.end());
    return r;
  }
  uint32_t scalar(napi_env env, napi_value js, bool lists) {
    auto v = GStruct<ZeGraphValue>();
    const auto type = GType(env, js);
    if (type == napi_null)
      v.tag = ZE_GRAPH_VALUE_NULL;
    else if (type == napi_boolean) {
      bool b;
      GCheck(env, napi_get_value_bool(env, js, &b));
      v.tag = ZE_GRAPH_VALUE_BOOL;
      v.boolean = b;
    } else if (type == napi_bigint) {
      bool exact;
      v.tag = ZE_GRAPH_VALUE_I64;
      GCheck(env, napi_get_value_bigint_int64(env, js, &v.integer, &exact));
      GRequire(env, exact, "I64 overflow");
    } else if (type == napi_number) {
      v.tag = ZE_GRAPH_VALUE_F64;
      v.floating = GDouble(env, js);
      GRequire(env, std::isfinite(v.floating), "nonfinite graph number");
    } else if (type == napi_string) {
      v.tag = ZE_GRAPH_VALUE_STRING;
      v.range = text(GText(env, js));
    } else {
      bool array;
      GCheck(env, napi_is_array(env, js, &array));
      GRequire(env, lists && array, "unsupported graph value");
      const uint32_t n = GLength(env, js);
      std::vector<uint32_t> entries;
      v.tag = ZE_GRAPH_VALUE_LIST;
      v.list_kind = ZE_GRAPH_LIST_EMPTY;
      for (uint32_t i = 0; i < n; ++i) {
        auto index = scalar(env, GAt(env, js, i), false);
        const auto tag = values.at(index).tag;
        GRequire(env, tag >= 1 && tag <= 4 && (i == 0 || tag == v.list_kind),
                 "property list must be homogeneous");
        v.list_kind = tag;
        entries.push_back(index);
      }
      v.range = {static_cast<uint32_t>(children.size()), n};
      children.insert(children.end(), entries.begin(), entries.end());
    }
    const auto index = static_cast<uint32_t>(values.size());
    values.push_back(v);
    return index;
  }
  // General query lists permit mixed scalars, nulls and nested lists.
  uint32_t parameter(napi_env env, napi_value js, unsigned depth = 0) {
    bool array, typed;
    GCheck(env, napi_is_array(env, js, &array));
    GCheck(env, napi_is_typedarray(env, js, &typed));
    if (!array && !typed)
      return scalar(env, js, false);
    GRequire(env, depth < 16,
             "parameter list depth exceeds 16 (or contains a cycle)");
    size_t count;
    if (typed) {
      napi_typedarray_type type;
      void *data;
      napi_value buffer;
      size_t offset;
      GCheck(env, napi_get_typedarray_info(env, js, &type, &count, &data,
                                           &buffer, &offset));
    } else
      count = GLength(env, js);
    GRequire(env, count <= 524288 - parameter_elements,
             "parameter lists exceed 524288 elements");
    parameter_elements += count;
    std::vector<uint32_t> entries;
    entries.reserve(count);
    for (size_t i = 0; i < count; ++i)
      entries.push_back(
          parameter(env, GAt(env, js, static_cast<uint32_t>(i)), depth + 1));
    auto v = GStruct<ZeGraphValue>();
    v.tag = ZE_GRAPH_VALUE_LIST;
    v.list_kind = ZE_GRAPH_LIST_QUERY;
    v.range = {static_cast<uint32_t>(children.size()),
               static_cast<uint32_t>(count)};
    children.insert(children.end(), entries.begin(), entries.end());
    const auto index = static_cast<uint32_t>(values.size());
    values.push_back(v);
    return index;
  }
  size_t parameter_elements = 0;
  ZeGraphRange props(napi_env env, napi_value obj) {
    ZeGraphRange range{static_cast<uint32_t>(properties.size()), 0};
    if (GType(env, obj) == napi_undefined)
      return range;
    napi_value keys;
    GCheck(env, napi_get_property_names(env, obj, &keys));
    range.count = GLength(env, keys);
    for (uint32_t i = 0; i < range.count; ++i) {
      const auto key = GAt(env, keys, i);
      napi_value value;
      GCheck(env, napi_get_property(env, obj, key, &value));
      auto prop = GStruct<ZeGraphProperty>();
      prop.name = text(GText(env, key));
      prop.value = scalar(env, value, true);
      properties.push_back(prop);
    }
    return range;
  }
  ZeGraphValuePool view() const {
    auto p = GStruct<ZeGraphValuePool>();
    p.bytes = bytes.data();
    p.byte_count = bytes.size();
    p.values = values.data();
    p.value_count = values.size();
    p.children = children.data();
    p.child_count = children.size();
    p.names = names.data();
    p.name_count = names.size();
    p.properties = properties.data();
    p.property_count = properties.size();
    p.nodes = nodes.data();
    p.node_count = nodes.size();
    p.vectors = vectors.data();
    p.vector_count = vectors.size();
    p.relationships = relationships.data();
    p.relationship_count = relationships.size();
    return p;
  }
};
ZeGraphEndpoint GEndpoint(napi_env env, napi_value value) {
  auto e = GStruct<ZeGraphEndpoint>();
  if (GType(env, value) == napi_bigint) {
    e.kind = ZE_GRAPH_ENDPOINT_NODE;
    e.node = GId(env, value);
  } else {
    e.kind = ZE_GRAPH_ENDPOINT_LOCAL;
    e.local_item =
        static_cast<uint32_t>(GDouble(env, GGet(env, value, "local")));
  }
  return e;
}
std::string GPoolText(const ZeGraphValuePool &p, ZeGraphRange r) {
  if (r.start > p.byte_count || r.count > p.byte_count - r.start)
    throw std::runtime_error("invalid graph response string");
  if (r.count == 0)
    return {};
  return std::string(reinterpret_cast<const char *>(p.bytes + r.start),
                     r.count);
}
template <typename T>
const T &GEntry(const T *ptr, size_t count, size_t index) {
  if (!ptr || index >= count)
    throw std::runtime_error("invalid graph response index");
  return ptr[index];
}
napi_value GValue(napi_env env, const ZeGraphValuePool &p, uint32_t index,
                  unsigned depth = 0);
napi_value GProperties(napi_env env, const ZeGraphValuePool &p,
                       ZeGraphRange range, unsigned depth) {
  auto result = GObject(env);
  for (uint64_t i = range.start; i < uint64_t(range.start) + range.count; ++i) {
    const auto &prop = GEntry(p.properties, p.property_count, i);
    // Define an own data property, including names such as __proto__ and
    // embedded NULs.
    napi_property_descriptor descriptor{};
    descriptor.name = GString(env, GPoolText(p, prop.name));
    descriptor.value = GValue(env, p, prop.value, depth + 1);
    descriptor.attributes = napi_default_jsproperty;
    GCheck(env, napi_define_properties(env, result, 1, &descriptor));
  }
  return result;
}
napi_value GValue(napi_env env, const ZeGraphValuePool &p, uint32_t index,
                  unsigned depth) {
  if (depth > 64)
    throw std::runtime_error("graph response nesting exceeds 64");
  const auto &v = GEntry(p.values, p.value_count, index);
  switch (v.tag) {
  case ZE_GRAPH_VALUE_NULL:
    return GNull(env);
  case ZE_GRAPH_VALUE_BOOL:
    return GBool(env, v.boolean != 0);
  case ZE_GRAPH_VALUE_I64: {
    napi_value result;
    GCheck(env, napi_create_bigint_int64(env, v.integer, &result));
    return result;
  }
  case ZE_GRAPH_VALUE_F64:
    return GNumber(env, v.floating);
  case ZE_GRAPH_VALUE_STRING:
    return GString(env, GPoolText(p, v.range));
  case ZE_GRAPH_VALUE_LIST: {
    auto arr = GArray(env, v.range.count);
    for (uint32_t i = 0; i < v.range.count; ++i)
      GElement(
          env, arr, i,
          GValue(env, p,
                 GEntry(p.children, p.child_count, size_t(v.range.start) + i),
                 depth + 1));
    return arr;
  }
  case ZE_GRAPH_VALUE_NODE: {
    const auto &n = GEntry(p.nodes, p.node_count, v.entity_index);
    auto obj = GObject(env);
    GSet(env, obj, "kind", GString(env, "node"));
    GSet(env, obj, "id", GIdValue(env, n.id.high, n.id.low));
    GSet(env, obj, "revision", GU64(env, n.revision));
    GSet(env, obj, "lastChangeGeneration", GU64(env, n.last_change_generation));
    if (n.has_key) {
      GSet(env, obj, "namespace", GString(env, GPoolText(p, n.namespace_name)));
      GSet(env, obj, "key", GString(env, GPoolText(p, n.key)));
    }
    if (n.has_text)
      GSet(env, obj, "text", GString(env, GPoolText(p, n.text)));
    if (n.has_vector) {
      if (n.vector.start > p.vector_count ||
          n.vector.count > p.vector_count - n.vector.start ||
          (n.vector.count && !p.vectors))
        throw std::runtime_error("invalid graph vector range");
      napi_value buffer, vector;
      void *ptr;
      GCheck(env,
             napi_create_arraybuffer(
                 env, size_t(n.vector.count) * sizeof(float), &ptr, &buffer));
      if (n.vector.count)
        std::memcpy(ptr, p.vectors + n.vector.start,
                    size_t(n.vector.count) * sizeof(float));
      GCheck(env, napi_create_typedarray(env, napi_float32_array,
                                         n.vector.count, buffer, 0, &vector));
      GSet(env, obj, "vector", vector);
    }
    GSet(env, obj, "properties", GProperties(env, p, n.properties, depth));
    auto labels = GArray(env, n.labels.count);
    for (uint32_t i = 0; i < n.labels.count; ++i)
      GElement(env, labels, i,
               GString(env, GPoolText(p, GEntry(p.names, p.name_count,
                                                size_t(n.labels.start) + i))));
    GSet(env, obj, "labels", labels);
    return obj;
  }
  case ZE_GRAPH_VALUE_RELATIONSHIP: {
    const auto &r =
        GEntry(p.relationships, p.relationship_count, v.entity_index);
    auto obj = GObject(env);
    GSet(env, obj, "kind", GString(env, "relationship"));
    GSet(env, obj, "id", GIdValue(env, r.id.high, r.id.low));
    GSet(env, obj, "source", GIdValue(env, r.source.high, r.source.low));
    GSet(env, obj, "target", GIdValue(env, r.target.high, r.target.low));
    GSet(env, obj, "type", GString(env, GPoolText(p, r.relationship_type)));
    GSet(env, obj, "revision", GU64(env, r.revision));
    GSet(env, obj, "lastChangeGeneration", GU64(env, r.last_change_generation));
    if (r.has_key) {
      GSet(env, obj, "namespace", GString(env, GPoolText(p, r.namespace_name)));
      GSet(env, obj, "key", GString(env, GPoolText(p, r.key)));
    }
    GSet(env, obj, "properties", GProperties(env, p, r.properties, depth));
    return obj;
  }
  default:
    throw std::runtime_error("invalid graph value tag");
  }
}
napi_value GResult(napi_env env, ze_handle handle, ze_error_code status,
                   const ZeGraphResponse &r,
                   const std::string *message = nullptr) {
  if (status != ZE_OK) {
    napi_value error;
    if (!(message
              ? CreateZeppelinErrorWithMessage(env, status, *message, &error)
              : CreateZeppelinError(env, handle, status, &error)))
      throw GraphPending{};
    GMetadata(env, error, r);
    GCheck(env, napi_throw(env, error));
    throw GraphPending{};
  }
  auto result = GObject(env);
  GMetadata(env, result, r);
  auto columns = GArray(env, r.column_count);
  auto rows = GArray(env, r.row_count);
  auto receipts = GArray(env, r.receipt_count);
  for (size_t i = 0; i < r.column_count; ++i)
    GElement(
        env, columns, i,
        GString(env,
                GPoolText(r.pool, GEntry(r.columns, r.column_count, i).name)));
  for (size_t i = 0; i < r.row_count; ++i) {
    auto row = GArray(env, r.column_count);
    for (size_t j = 0; j < r.column_count; ++j)
      GElement(env, row, j,
               GValue(env, r.pool,
                      GEntry(r.cells, r.cell_count, i * r.column_count + j)));
    GElement(env, rows, i, row);
  }
  for (size_t i = 0; i < r.receipt_count; ++i) {
    const auto &receipt = GEntry(r.receipts, r.receipt_count, i);
    auto obj = GObject(env);
    GSet(env, obj, "item", GNumber(env, receipt.item));
    GSet(env, obj, "kind",
         GString(env, receipt.entity_kind == 0 ? "node" : "relationship"));
    GSet(env, obj, "id",
         receipt.entity_kind == 0
             ? GIdValue(env, receipt.node.high, receipt.node.low)
             : GIdValue(env, receipt.relationship.high,
                        receipt.relationship.low));
    GSet(env, obj, "disposition",
         GString(env, GDisposition(receipt.disposition)));
    GSet(env, obj, "deleted", GBool(env, receipt.deleted != 0));
    GSet(env, obj, "revision", GU64(env, receipt.revision));
    GSet(env, obj, "generation", GU64(env, receipt.generation));
    GElement(env, receipts, i, obj);
  }
  GSet(env, result, "columns", columns);
  GSet(env, result, "rows", rows);
  GSet(env, result, "receipts", receipts);
  return result;
}
template <bool Async = false>
napi_value GraphApply(napi_env env, napi_callback_info info) {
  return GGuard(env, [&]() {
    napi_value args[2], receiver;
    size_t argc = 2;
    GCheck(env, napi_get_cb_info(env, info, &argc, args, &receiver, nullptr));
    GRequire(env, argc == 2, "graphApply arguments");
    const auto handle = GHandle(env, receiver);
    struct Data {
      GPool pool;
      std::vector<ZeGraphBatchItem> items;
      std::vector<ZeStoreGraphDocument> documents;
      std::vector<std::vector<uint8_t>> metadata;
      std::vector<std::vector<ZeAttributeValue>> attributes;
      std::vector<std::vector<std::string>> strings;
      ZeGraphControl control = GStruct<ZeGraphControl>();
      GResponse response;
    };
    auto data = std::make_shared<Data>();
    auto &pool = data->pool;
    auto &items = data->items;
    const auto count = GLength(env, args[0]);
    items.reserve(count);
    data->documents.reserve(count);
    data->metadata.resize(count);
    data->attributes.resize(count);
    data->strings.resize(count);
    data->control = GControl(env, args[1]);
    for (uint32_t i = 0; i < count; ++i) {
      auto js = GAt(env, args[0], i);
      auto item = GStruct<ZeGraphBatchItem>();
      item.source = GStruct<ZeGraphEndpoint>();
      item.target = GStruct<ZeGraphEndpoint>();
      item.entity_kind = GText(env, GGet(env, js, "kind")) == "node" ? 0 : 1;
      const auto op = GText(env, GGet(env, js, "operation"));
      GRequire(env,
               op == "create" || op == "put" || op == "delete" ||
                   op == "recreate",
               "invalid graph operation");
      item.operation = op == "create"   ? 0
                       : op == "put"    ? 1
                       : op == "delete" ? 2
                                        : 3;
      item.namespace_name = pool.text(GText(env, GGet(env, js, "namespace")));
      item.key = pool.text(GText(env, GGet(env, js, "key")));
      item.revision = GInteger(env, GGet(env, js, "revision"));
      if (item.operation == 1 || item.operation == 2) {
        const auto id = GId(env, GGet(env, js, "expectedId"));
        if (item.entity_kind == 0)
          item.expected_node = id;
        else
          item.expected_relationship = {id.high, id.low};
      }
      if (item.operation == 3)
        item.expected_deletion_revision =
            GInteger(env, GGet(env, js, "expectedDeletionRevision"));
      if (item.operation == 2) {
        auto detach = GGet(env, js, "detach");
        if (GType(env, detach) != napi_undefined) {
          bool b;
          GCheck(env, napi_get_value_bool(env, detach, &b));
          item.delete_mode = b;
        }
      } else {
        item.has_image = 1;
        auto properties = pool.props(env, GGet(env, js, "properties"));
        if (item.entity_kind == 0) {
          auto n = GStruct<ZeGraphNode>();
          n.properties = properties;
          auto text = GGet(env, js, "text");
          if (GType(env, text) != napi_undefined) {
            n.has_text = 1;
            n.text = pool.text(GText(env, text));
          }
          auto vector = GGet(env, js, "vector");
          if (GType(env, vector) != napi_undefined) {
            const float *ptr;
            size_t length;
            if (!GetFloat32Array(env, vector, "node vector", &ptr, &length))
              throw GraphPending{};
            n.has_vector = 1;
            n.vector = {static_cast<uint32_t>(pool.vectors.size()),
                        static_cast<uint32_t>(length)};
            if (length)
              pool.vectors.insert(pool.vectors.end(), ptr, ptr + length);
          }
          bool supplemental = false;
          for (const char *name : {"id", "timestamp", "attributes", "metadata"})
            supplemental |= GType(env, GGet(env, js, name)) != napi_undefined;
          if (supplemental) {
            auto doc = GStruct<ZeStoreGraphDocument>();
            doc.item_index = i;
            auto id = GGet(env, js, "id");
            if (GType(env, id) != napi_undefined) {
              auto value = GId(env, id);
              doc.has_id = 1;
              doc.id = {value.high, value.low};
            }
            if (!GetOptionalBigInt64(env, js, "timestamp", 0, &doc.timestamp))
              throw GraphPending{};
            auto meta = GGet(env, js, "metadata");
            if (GType(env, meta) != napi_undefined &&
                !GetUint8ArrayCopy(env, meta, "node metadata",
                                   &data->metadata[i]))
              throw GraphPending{};
            doc.metadata = data->metadata[i].data();
            doc.metadata_len = data->metadata[i].size();
            auto attrs = GGet(env, js, "attributes");
            if (GType(env, attrs) != napi_undefined) {
              const auto count = GLength(env, attrs);
              data->attributes[i].resize(count);
              data->strings[i].resize(count);
              for (uint32_t j = 0; j < count; ++j)
                if (!ParseAttributeValue(env, GAt(env, attrs, j),
                                         &data->attributes[i][j],
                                         &data->strings[i][j]))
                  throw GraphPending{};
            }
            doc.attributes = data->attributes[i].data();
            doc.attribute_count = data->attributes[i].size();
            data->documents.push_back(doc);
          }
          auto labels = GGet(env, js, "labels");
          n.labels.start = pool.names.size();
          if (GType(env, labels) != napi_undefined) {
            n.labels.count = GLength(env, labels);
            for (uint32_t j = 0; j < n.labels.count; ++j)
              pool.names.push_back(pool.text(GText(env, GAt(env, labels, j))));
          }
          item.image = pool.nodes.size();
          pool.nodes.push_back(n);
        } else {
          auto r = GStruct<ZeGraphRelationship>();
          r.properties = properties;
          r.relationship_type = pool.text(GText(env, GGet(env, js, "type")));
          item.image = pool.relationships.size();
          pool.relationships.push_back(r);
          item.source = GEndpoint(env, GGet(env, js, "source"));
          item.target = GEndpoint(env, GGet(env, js, "target"));
        }
      }
      items.push_back(item);
    }
    return RunNative<Async>(
        env, receiver, handle,
        [data, handle]() {
          auto view = data->pool.view();
          auto request = GStruct<ZeGraphBatchRequest>();
          request.items = data->items.data();
          request.item_count = data->items.size();
          request.pool = &view;
          request.control = &data->control;
          auto v2 = GStruct<ZeStoreGraphBatchRequestV2>();
          v2.graph = &request;
          v2.documents = data->documents.data();
          v2.document_count = data->documents.size();
          return ze_store_graph_apply_v2(handle, &v2, &data->response.value);
        },
        [data, env, handle](ze_error_code status, const std::string &message) {
          return GGuard(env, [&]() {
            return GResult(env, handle, status, data->response.value, &message);
          });
        });
  });
}
template <bool Async = false>
napi_value GraphCypher(napi_env env, napi_callback_info info) {
  return GGuard(env, [&]() {
    napi_value args[4], receiver;
    size_t argc = 4;
    GCheck(env, napi_get_cb_info(env, info, &argc, args, &receiver, nullptr));
    GRequire(env, argc == 4, "graphCypher arguments");
    const auto handle = GHandle(env, receiver);
    struct Data {
      std::string text;
      GPool pool;
      std::vector<ZeGraphParameterValue> parameters;
      GResponse response;
      ZeGraphControl control = GStruct<ZeGraphControl>();
    };
    auto data = std::make_shared<Data>();
    data->text = GText(env, args[0]);
    auto &pool = data->pool;
    auto &parameters = data->parameters;
    data->control = GControl(env, args[3]);
    napi_value names;
    GCheck(env, napi_get_property_names(env, args[1], &names));
    for (uint32_t i = 0; i < GLength(env, names); ++i) {
      auto name = GAt(env, names, i);
      napi_value value;
      GCheck(env, napi_get_property(env, args[1], name, &value));
      auto param = GStruct<ZeGraphParameterValue>();
      param.name = pool.text(GText(env, name));
      param.value = pool.parameter(env, value);
      parameters.push_back(param);
    }
    const auto max_rows = GDouble(env, args[2]);
    GRequire(env,
             std::isfinite(max_rows) && max_rows >= 0 && max_rows <= 65536 &&
                 std::floor(max_rows) == max_rows,
             "invalid maxRows");
    return RunNative<Async>(
        env, receiver, handle,
        [data, handle, max_rows]() {
          auto view = data->pool.view();
          auto request = GStruct<ZeGraphCypherRequest>();
          request.query = {reinterpret_cast<const uint8_t *>(data->text.data()),
                           data->text.size()};
          request.parameters = data->parameters.data();
          request.parameter_count = data->parameters.size();
          request.parameter_pool = &view;
          request.control = &data->control;
          return ze_store_cypher_with_row_limit(handle, &request,
                                                static_cast<uint32_t>(max_rows),
                                                &data->response.value);
        },
        [data, env, handle](ze_error_code status, const std::string &message) {
          return GGuard(env, [&]() {
            return GResult(env, handle, status, data->response.value, &message);
          });
        });
  });
}
napi_value GraphSetMaintenancePolicy(napi_env env, napi_callback_info info) {
  return GGuard(env, [&]() -> napi_value {
    napi_value args[2], receiver;
    size_t argc = 2;
    GCheck(env, napi_get_cb_info(env, info, &argc, args, &receiver, nullptr));
    GRequire(env, argc == 2, "graphSetMaintenancePolicy arguments");
    const auto handle = GHandle(env, receiver);
    bool automatic;
    GCheck(env, napi_get_value_bool(env, args[0], &automatic));
    const double bytes = GDouble(env, args[1]);
    GRequire(env,
             std::isfinite(bytes) && bytes >= 1048576 &&
                 bytes <= 9007199254740991.0 && std::floor(bytes) == bytes,
             "invalid reclaimAfterBytes");
    auto policy = GStruct<ZeGraphMaintenancePolicy>();
    policy.automatic = automatic;
    policy.reclaim_after_bytes = static_cast<uint64_t>(bytes);
    const auto status = ze_store_set_graph_maintenance_policy(handle, &policy);
    if (status != ZE_OK)
      return ThrowZeppelin(env, handle, status);
    napi_value result;
    GCheck(env, napi_get_undefined(env, &result));
    return result;
  });
}
template <bool Async = false>
napi_value GraphMaintain(napi_env env, napi_callback_info info) {
  return GGuard(env, [&]() {
    napi_value arg, receiver;
    size_t argc = 1;
    GCheck(env, napi_get_cb_info(env, info, &argc, &arg, &receiver, nullptr));
    GRequire(env, argc == 1, "graphMaintain arguments");
    const auto handle = GHandle(env, receiver);
    auto report = std::make_shared<ZeGraphMaintainReport>(
        GStruct<ZeGraphMaintainReport>());
    auto control = std::make_shared<ZeGraphControl>(GControl(env, arg));
    return RunNative<Async>(
        env, receiver, handle,
        [report, handle, control]() {
          return ze_store_graph_maintain(handle, control.get(), report.get());
        },
        [report, env](ze_error_code status, const std::string &message) {
          return GGuard(env, [&]() -> napi_value {
            if (status != ZE_OK)
              return ThrowWorkerError(env, status, message);
            auto result = GObject(env);
            GSet(env, result, "cycleComplete",
                 GBool(env, report->cycle_complete != 0));
            GSet(env, result, "generation", GU64(env, report->generation));
            GSet(env, result, "replacedPhysicalRefs",
                 GU64(env, report->replaced_physical_refs));
            GSet(env, result, "newPackBytes",
                 GU64(env, report->new_pack_bytes));
            GSet(env, result, "relocatedBytes",
                 GU64(env, report->relocated_bytes));
            GSet(env, result, "drainedPacks", GU64(env, report->drained_packs));
            GSet(env, result, "reclaimedBytes",
                 GU64(env, report->reclaimed_bytes));
            GSet(env, result, "removedBytes", GU64(env, report->removed_bytes));
            return result;
          });
        });
  });
}

#include "graph_plan.inc"
napi_value StoreEnableGraph(napi_env env, napi_callback_info info) {
  return GGuard(env, [&]() -> napi_value {
    napi_value receiver;
    size_t argc = 0;
    GCheck(env,
           napi_get_cb_info(env, info, &argc, nullptr, &receiver, nullptr));
    const auto handle = GHandle(env, receiver);
    auto report = GStruct<ZeGenerationReport>();
    const auto status = ze_store_enable_graph(handle, &report);
    if (status != ZE_OK)
      return ThrowZeppelin(env, handle, status);
    auto result = GObject(env);
    GSet(env, result, "generation", GU64(env, report.generation));
    return result;
  });
}
napi_value GraphResources(napi_env env, napi_callback_info info) {
  return GGuard(env, [&]() -> napi_value {
    napi_value receiver;
    size_t argc = 0;
    GCheck(env,
           napi_get_cb_info(env, info, &argc, nullptr, &receiver, nullptr));
    const auto handle = GHandle(env, receiver);
    auto r = GStruct<ZeGraphResources>();
    const auto status = ze_store_graph_resources(handle, &r);
    if (status != ZE_OK)
      return ThrowZeppelin(env, handle, status);
    auto obj = GObject(env);
    GSet(env, obj, "engineBytes", GU64(env, r.engine_bytes));
    GSet(env, obj, "enginePeakBytes", GU64(env, r.engine_peak_bytes));
    GSet(env, obj, "applicationBytes", GU64(env, r.application_bytes));
    GSet(env, obj, "applicationPeakBytes", GU64(env, r.application_peak_bytes));
    return obj;
  });
}
template <bool Nodes, bool Async>
napi_value GraphGet(napi_env env, napi_callback_info info) {
  return GGuard(env, [&]() {
    napi_value args[4], receiver;
    size_t argc = 4;
    GCheck(env, napi_get_cb_info(env, info, &argc, args, &receiver, nullptr));
    GRequire(env, argc == 4, "graph getter arguments");
    const auto handle = GHandle(env, receiver);
    struct Data {
      std::vector<ZeNodeId> nodes;
      std::vector<ZeRelId> rels;
      ZeGraphControl control = GStruct<ZeGraphControl>();
      GResponse response;
    };
    auto data = std::make_shared<Data>();
    const auto count = GLength(env, args[0]);
    for (uint32_t i = 0; i < count; ++i) {
      auto id = GId(env, GAt(env, args[0], i));
      if constexpr (Nodes)
        data->nodes.push_back(id);
      else
        data->rels.push_back({id.high, id.low});
    }
    const bool text = GBoolean(env, args[1]), vector = GBoolean(env, args[2]);
    data->control = GControl(env, args[3]);
    return RunNative<Async>(
        env, receiver, handle,
        [data, handle, text, vector]() {
          (void)text;
          (void)vector;
          if constexpr (Nodes) {
            auto r = GStruct<ZeGraphGetNodesRequest>();
            r.ids = data->nodes.data();
            r.id_count = data->nodes.size();
            r.include_text = text;
            r.include_vector = vector;
            r.control = &data->control;
            return ze_store_get_nodes(handle, &r, &data->response.value);
          } else {
            auto r = GStruct<ZeGraphGetRelsRequest>();
            r.ids = data->rels.data();
            r.id_count = data->rels.size();
            r.control = &data->control;
            return ze_store_get_relationships(handle, &r,
                                              &data->response.value);
          }
        },
        [data, env, handle, count](ze_error_code status,
                                   const std::string &message) {
          return GGuard(env, [&]() {
            if (status == ZE_OK && (data->response.value.row_count != count ||
                                    data->response.value.column_count != 1))
              throw std::runtime_error("invalid ordered getter response");
            return GResult(env, handle, status, data->response.value, &message);
          });
        });
  });
}
template <bool Async>
napi_value GraphQuery(napi_env env, napi_callback_info info) {
  return GGuard(env, [&]() {
    napi_value args[3], receiver;
    size_t argc = 3;
    GCheck(env, napi_get_cb_info(env, info, &argc, args, &receiver, nullptr));
    GRequire(env, argc == 3, "graphQuery arguments");
    const auto handle = GHandle(env, receiver);
    struct Data {
      GPlan plan;
      GPool pool;
      std::vector<ZeGraphParameterValue> parameters;
      ZeGraphControl control = GStruct<ZeGraphControl>();
      GResponse response;
    };
    auto data = std::make_shared<Data>();
    data->plan.read(env, args[0]);
    data->control = GControl(env, args[2]);
    napi_value names;
    GCheck(env, napi_get_property_names(env, args[1], &names));
    for (uint32_t i = 0; i < GLength(env, names); ++i) {
      auto name = GAt(env, names, i);
      napi_value value;
      GCheck(env, napi_get_property(env, args[1], name, &value));
      auto p = GStruct<ZeGraphParameterValue>();
      p.name = data->pool.text(GText(env, name));
      p.value = data->pool.parameter(env, value);
      data->parameters.push_back(p);
    }
    return RunNative<Async>(
        env, receiver, handle,
        [data, handle]() {
          auto pool = data->plan.pool.view();
          auto plan = data->plan.view(&pool);
          auto params = data->pool.view();
          auto r = GStruct<ZeGraphQueryRequest>();
          r.plan = &plan;
          r.parameters = data->parameters.data();
          r.parameter_count = data->parameters.size();
          r.parameter_pool = &params;
          r.control = &data->control;
          return ze_store_graph_query(handle, &r, &data->response.value);
        },
        [data, env, handle](ze_error_code status, const std::string &message) {
          return GGuard(env, [&]() {
            return GResult(env, handle, status, data->response.value, &message);
          });
        });
  });
}

#endif
