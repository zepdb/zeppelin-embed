// Owned structured-plan arenas. Included by graph.inc under ZE_GRAPH.
uint32_t GIndex(napi_env env, napi_value v) {
  auto n = GDouble(env, v);
  GRequire(env,
           std::isfinite(n) && n >= 0 && n <= UINT32_MAX && std::floor(n) == n,
           "plan index must be uint32");
  return static_cast<uint32_t>(n);
}
bool GBoolean(napi_env env, napi_value v) {
  bool b;
  GCheck(env, napi_get_value_bool(env, v, &b));
  return b;
}
ZeGraphOptionalIndex GOptional(napi_env env, napi_value v) {
  if (GType(env, v) == napi_null || GType(env, v) == napi_undefined)
    return {};
  return {1, GIndex(env, v)};
}
std::string
GTag(napi_env env, napi_value tuple,
     std::initializer_list<std::pair<const char *, uint32_t>> cases) {
  const auto tag = GText(env, GAt(env, tuple, 0));
  for (const auto &entry : cases) {
    if (tag == entry.first) {
      GRequire(env, GLength(env, tuple) == entry.second,
               "invalid plan tuple arity");
      return tag;
    }
  }
  GRequire(env, false, "unknown plan tuple");
  return {};
}
struct GPlan {
  GPool pool;
  uint32_t root{};
  std::vector<ZeGraphOperator> operators;
  std::vector<ZeGraphExpression> expressions;
  std::vector<uint32_t> inputs, children, eager;
  std::vector<ZeGraphProjection> projections;
  std::vector<ZeGraphSortKey> sorts;
  std::vector<ZeGraphMutation> mutations;
  std::vector<ZeGraphParameter> parameters;
  std::vector<ZeGraphSearch> searches;
  std::vector<ZeGraphSearchOptions> options;
  ZeGraphRange names(napi_env env, napi_value js) {
    ZeGraphRange r{static_cast<uint32_t>(pool.names.size()), GLength(env, js)};
    for (uint32_t i = 0; i < r.count; ++i)
      pool.names.push_back(pool.text(GText(env, GAt(env, js, i))));
    return r;
  }
  ZeGraphRange bindings(napi_env env, napi_value js) {
    ZeGraphRange r{static_cast<uint32_t>(projections.size()), GLength(env, js)};
    for (uint32_t i = 0; i < r.count; ++i) {
      auto b = GAt(env, js, i);
      auto p = GStruct<ZeGraphProjection>();
      p.slot = GIndex(env, GGet(env, b, "slot"));
      p.expression = GIndex(env, GGet(env, b, "expression"));
      projections.push_back(p);
    }
    return r;
  }
  ZeGraphExpression literal(napi_env env, napi_value js, unsigned depth = 0) {
    bool array, typed;
    GCheck(env, napi_is_array(env, js, &array));
    GCheck(env, napi_is_typedarray(env, js, &typed));
    auto e = GStruct<ZeGraphExpression>();
    if (!array && !typed) {
      e.value = pool.scalar(env, js, false);
      return e;
    }
    GRequire(env, depth < 16, "literal list depth exceeds 16");
    size_t count = 0;
    if (typed) {
      napi_typedarray_type type;
      void *ptr;
      napi_value buffer;
      size_t offset;
      GCheck(env, napi_get_typedarray_info(env, js, &type, &count, &ptr,
                                           &buffer, &offset));
    } else
      count = GLength(env, js);
    GRequire(env, count <= 4096, "literal list exceeds expression arena");
    std::vector<uint32_t> ids;
    for (uint32_t i = 0; i < count; ++i) {
      auto child = literal(env, GAt(env, js, i), depth + 1);
      GRequire(env, expressions.size() < 4096, "expression arena exceeds 4096");
      ids.push_back(static_cast<uint32_t>(expressions.size()));
      expressions.push_back(child);
    }
    e.kind = 7;
    e.children = {static_cast<uint32_t>(children.size()),
                  static_cast<uint32_t>(ids.size())};
    children.insert(children.end(), ids.begin(), ids.end());
    return e;
  }
  void read(napi_env env, napi_value js) {
    root = GIndex(env, GGet(env, js, "root"));
    auto es = GGet(env, js, "expressions");
    const auto ec = GLength(env, es);
    GRequire(env, ec <= 4096, "expression arena exceeds 4096");
    expressions.resize(ec);
    for (uint32_t i = 0; i < ec; ++i) {
      auto tuple = GAt(env, es, i);
      auto tag = GTag(env, tuple,
                      {{"literal", 2},
                       {"slot", 2},
                       {"parameter", 2},
                       {"unary", 3},
                       {"binary", 4},
                       {"property", 3},
                       {"hasLabel", 3},
                       {"list", 2},
                       {"aggregate", 4}});
      auto e = GStruct<ZeGraphExpression>();
      auto a = [&](uint32_t n) { return GAt(env, tuple, n); };
      if (tag == "literal")
        e = literal(env, a(1));
      else if (tag == "slot" || tag == "parameter") {
        e.kind = tag == "slot" ? 1 : 2;
        e.value = GIndex(env, a(1));
      } else if (tag == "unary" || tag == "binary") {
        e.kind = tag == "unary" ? 3 : 4;
        e.operation = GIndex(env, a(1));
        e.left = GIndex(env, a(2));
        if (e.kind == 4)
          e.right = GIndex(env, a(3));
      } else if (tag == "property" || tag == "hasLabel") {
        e.kind = tag == "property" ? 5 : 6;
        e.left = GIndex(env, a(1));
        e.name = pool.text(GText(env, a(2)));
      } else if (tag == "list") {
        e.kind = 7;
        auto list = a(1);
        e.children = {static_cast<uint32_t>(children.size()),
                      GLength(env, list)};
        for (uint32_t j = 0; j < e.children.count; ++j)
          children.push_back(GIndex(env, GAt(env, list, j)));
      } else if (tag == "aggregate") {
        e.kind = 8;
        e.operation = GIndex(env, a(1));
        auto operand = GOptional(env, a(2));
        e.has_operand = operand.present;
        e.left = operand.index;
        e.distinct = GBoolean(env, a(3));
      } else
        GRequire(env, false, "unknown expression");
      expressions[i] = e;
    }
    auto ops = GGet(env, js, "operators");
    GRequire(env, GLength(env, ops) <= 4096, "operator arena exceeds 4096");
    for (uint32_t i = 0; i < GLength(env, ops); ++i) {
      auto tuple = GAt(env, ops, i);
      auto tag =
          GTag(env, tuple, {{"unit", 1},          {"join", 4},
                            {"optionalApply", 4}, {"distinct", 2},
                            {"eager", 2},         {"collect", 2},
                            {"sort", 3},          {"scanNodes", 3},
                            {"mutate", 3},        {"aggregate", 4},
                            {"search", 3},        {"offsetLimit", 4},
                            {"lookupNode", 3},    {"lookupRelationship", 3},
                            {"lookupKey", 5},     {"expand", 3},
                            {"boundedExpand", 6}, {"project", 3},
                            {"with", 3},          {"filter", 3},
                            {"eligibleSet", 4}});
      auto o = GStruct<ZeGraphOperator>();
      std::vector<uint32_t> deps;
      auto a = [&](uint32_t n) { return GAt(env, tuple, n); };
      auto input = [&]() { deps.push_back(GIndex(env, a(1))); };
      if (tag == "unit")
        o.kind = 0;
      else if (tag == "join" || tag == "optionalApply") {
        o.kind = tag == "join" ? 1 : 15;
        deps = {GIndex(env, a(1)), GIndex(env, a(2))};
        o.predicate = GOptional(env, a(3));
      } else if (tag == "distinct" || tag == "eager" || tag == "collect") {
        o.kind = tag == "distinct" ? 2 : tag == "eager" ? 5 : 19;
        input();
      } else if (tag == "sort") {
        o.kind = 3;
        input();
        auto keys = a(2);
        o.sort_keys = {static_cast<uint32_t>(sorts.size()), GLength(env, keys)};
        for (uint32_t j = 0; j < o.sort_keys.count; ++j) {
          auto key = GAt(env, keys, j);
          auto s = GStruct<ZeGraphSortKey>();
          s.expression = GIndex(env, GGet(env, key, "expression"));
          s.descending = GBoolean(env, GGet(env, key, "descending"));
          sorts.push_back(s);
        }
      } else if (tag == "scanNodes") {
        o.kind = 4;
        o.node_slot = GIndex(env, a(1));
        if (GType(env, a(2)) != napi_null) {
          o.has_name = 1;
          o.name = pool.text(GText(env, a(2)));
        }
      } else if (tag == "mutate") {
        o.kind = 6;
        input();
        auto list = a(2);
        o.mutations = {static_cast<uint32_t>(mutations.size()),
                       GLength(env, list)};
        for (uint32_t j = 0; j < o.mutations.count; ++j) {
          auto t = GAt(env, list, j);
          auto name = GTag(env, t,
                           {{"createNode", 3},
                            {"createRelationship", 5},
                            {"removeProperty", 3},
                            {"setLabel", 4},
                            {"delete", 3},
                            {"setProperty", 4}});
          auto m = GStruct<ZeGraphMutation>();
          auto v = [&](uint32_t n) { return GAt(env, t, n); };
          if (name == "createNode") {
            m.kind = 0;
            m.output = GIndex(env, v(1));
            m.labels = names(env, v(2));
          } else if (name == "createRelationship") {
            m.kind = 1;
            m.output = GIndex(env, v(1));
            m.source = GIndex(env, v(2));
            m.target = GIndex(env, v(3));
            m.name = pool.text(GText(env, v(4)));
          } else if (name == "removeProperty" || name == "setLabel" ||
                     name == "setProperty") {
            m.kind = name == "removeProperty" ? 2 : name == "setLabel" ? 3 : 5;
            m.entity = GIndex(env, v(1));
            m.name = pool.text(GText(env, v(2)));
            if (m.kind == 3)
              m.present = GBoolean(env, v(3));
            if (m.kind == 5)
              m.value = GIndex(env, v(3));
          } else if (name == "delete") {
            m.kind = 4;
            m.entity = GIndex(env, v(1));
            m.detach = GBoolean(env, v(2));
          } else
            GRequire(env, false, "unknown plan mutation");
          mutations.push_back(m);
        }
      } else if (tag == "aggregate") {
        o.kind = 7;
        input();
        o.projections = bindings(env, a(2));
        o.aggregates = bindings(env, a(3));
      } else if (tag == "search") {
        o.kind = 8;
        o.search = GIndex(env, a(1));
        auto eligibility = GOptional(env, a(2));
        if (eligibility.present)
          deps.push_back(eligibility.index);
      } else if (tag == "offsetLimit") {
        o.kind = 9;
        input();
        o.offset = GInteger(env, a(2));
        if (GType(env, a(3)) != napi_null) {
          o.has_limit = 1;
          o.limit = GInteger(env, a(3));
        }
      } else if (tag == "lookupNode" || tag == "lookupRelationship") {
        o.kind = tag == "lookupNode" ? 10 : 11;
        auto id = GId(env, a(2));
        if (o.kind == 10) {
          o.node_slot = GIndex(env, a(1));
          o.node_id = id;
        } else {
          o.relationship_slot = GIndex(env, a(1));
          o.relationship_id = {id.high, id.low};
        }
      } else if (tag == "lookupKey") {
        o.kind = 12;
        o.node_slot = GIndex(env, a(1));
        o.entity_kind = GIndex(env, a(2));
        o.has_name = 1;
        o.name = pool.text(GText(env, a(3)));
        o.key_expression = GIndex(env, a(4));
      } else if (tag == "expand" || tag == "boundedExpand") {
        o.kind = tag == "expand" ? 13 : 14;
        input();
        auto spec = a(2);
        o.source_slot = GIndex(env, GGet(env, spec, "source"));
        o.node_slot = GIndex(env, GGet(env, spec, "node"));
        o.relationship_slot = GIndex(env, GGet(env, spec, "relationship"));
        o.direction = GIndex(env, GGet(env, spec, "direction"));
        o.pattern = GIndex(env, GGet(env, spec, "pattern"));
        o.relationship_types = names(env, GGet(env, spec, "types"));
        if (o.kind == 14) {
          o.path_min = GIndex(env, a(3));
          o.path_max = GIndex(env, a(4));
          auto pred = a(5);
          if (GType(env, pred) != napi_null) {
            o.edge_predicate = {1, GIndex(env, GGet(env, pred, "expression"))};
            o.edge_slot = GIndex(env, GGet(env, pred, "slot"));
          }
        }
      } else if (tag == "project" || tag == "with") {
        o.kind = tag == "project" ? 16 : 17;
        input();
        o.projections = bindings(env, a(2));
      } else if (tag == "filter") {
        o.kind = 18;
        input();
        o.predicate = {1, GIndex(env, a(2))};
      } else if (tag == "eligibleSet") {
        o.kind = 20;
        input();
        o.source_slot = GIndex(env, a(2));
        o.set_slot = GIndex(env, a(3));
      } else
        GRequire(env, false, "unknown plan operator");
      if (!deps.empty())
        o.inputs = {static_cast<uint32_t>(inputs.size()),
                    static_cast<uint32_t>(deps.size())};
      inputs.insert(inputs.end(), deps.begin(), deps.end());
      operators.push_back(o);
    }
    auto ps = GGet(env, js, "parameters");
    GRequire(env, GLength(env, ps) <= 256, "too many plan parameters");
    for (uint32_t i = 0; i < GLength(env, ps); ++i) {
      auto js = GAt(env, ps, i);
      auto p = GStruct<ZeGraphParameter>();
      p.name = pool.text(GText(env, GGet(env, js, "name")));
      p.kinds = GIndex(env, GGet(env, js, "kinds"));
      parameters.push_back(p);
    }
    auto ss = GGet(env, js, "searches");
    GRequire(env, GLength(env, ss) <= 4096, "too many plan searches");
    options.resize(GLength(env, ss));
    for (uint32_t i = 0; i < GLength(env, ss); ++i) {
      auto js = GAt(env, ss, i);
      auto s = GStruct<ZeGraphSearch>();
      auto kind = GGet(env, js, "kind");
      auto tag = GTag(env, kind, {{"vector", 2}, {"text", 2}, {"hybrid", 3}});
      GRequire(env, tag == "vector" || tag == "text" || tag == "hybrid",
               "unknown search kind");
      s.kind = tag == "vector" ? 0 : tag == "text" ? 1 : 2;
      if (s.kind != 1)
        s.vector = {1, GIndex(env, GAt(env, kind, 1))};
      if (s.kind != 0)
        s.text = {1, GIndex(env, GAt(env, kind, s.kind == 1 ? 1 : 2))};
      s.call_id = GIndex(env, GGet(env, js, "call"));
      s.k = GIndex(env, GGet(env, js, "k"));
      auto tier = GOptional(env, GGet(env, js, "tier"));
      s.has_tier = tier.present;
      s.tier = tier.index;
      s.eligible_set = GOptional(env, GGet(env, js, "eligibleSet"));
      s.window = GOptional(env, GGet(env, js, "window"));
      s.node_slot = GIndex(env, GGet(env, js, "node"));
      s.score_slot = GIndex(env, GGet(env, js, "score"));
      s.vector_distance_slot = GOptional(env, GGet(env, js, "vectorDistance"));
      s.lexical_score_slot = GOptional(env, GGet(env, js, "lexicalScore"));
      auto opt = GGet(env, js, "options");
      if (GType(env, opt) != napi_null && GType(env, opt) != napi_undefined) {
        auto &o = options[i];
        o = GStruct<ZeGraphSearchOptions>();
        o.graph_profile = GIndex(env, GGet(env, opt, "profile"));
        o.graph_ef = GIndex(env, GGet(env, opt, "ef"));
        o.graph_seed = GInteger(env, GGet(env, opt, "seed"));
        o.lexical_flags = GBoolean(env, GGet(env, opt, "lastAsPrefix"));
        o.rescore = GIndex(env, GGet(env, opt, "rescore"));
        auto alpha = GGet(env, opt, "alpha");
        if (GType(env, alpha) != napi_null) {
          o.has_alpha = 1;
          o.alpha = GDouble(env, alpha);
        }
        o.rules_enabled = GBoolean(env, GGet(env, opt, "rulesEnabled"));
        auto rounds = GGet(env, opt, "maxRounds");
        if (GType(env, rounds) != napi_null) {
          o.has_max_rounds = 1;
          o.max_rounds = GInteger(env, rounds);
        }
        s.options = &o;
      }
      searches.push_back(s);
    }
    auto eg = GGet(env, js, "eagerSearches");
    GRequire(env, GLength(env, eg) <= 4096, "too many eager searches");
    for (uint32_t i = 0; i < GLength(env, eg); ++i)
      eager.push_back(GIndex(env, GAt(env, eg, i)));
  }
  ZeGraphPlan view(const ZeGraphValuePool *p) const {
    auto v = GStruct<ZeGraphPlan>();
    v.root = root;
    v.pool = p;
    v.operators = operators.data();
    v.operator_count = operators.size();
    v.expressions = expressions.data();
    v.expression_count = expressions.size();
    v.inputs = inputs.data();
    v.input_count = inputs.size();
    v.expression_children = children.data();
    v.expression_child_count = children.size();
    v.projections = projections.data();
    v.projection_count = projections.size();
    v.sort_keys = sorts.data();
    v.sort_key_count = sorts.size();
    v.mutations = mutations.data();
    v.mutation_count = mutations.size();
    v.parameters = parameters.data();
    v.parameter_count = parameters.size();
    v.searches = searches.data();
    v.search_count = searches.size();
    v.eager_searches = eager.data();
    v.eager_search_count = eager.size();
    return v;
  }
};
