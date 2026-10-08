#if ZE_GRAPH
import CZeppelinEmbed

private func optional(_ value: UInt32?) -> ZeGraphOptionalIndex {
  ZeGraphOptionalIndex(present: value == nil ? 0 : 1, index: value ?? 0)
}

// Flatten only; native code is the single semantic validator. Every descriptor
// starts zeroed, so inactive fields and reserved words retain the ABI contract.
final class GraphPlanEncoder {
  let pool = GraphEncoder()
  var inputs: [UInt32] = [], children: [UInt32] = []
  var projections: [ZeGraphProjection] = [], sorts: [ZeGraphSortKey] = []
  var mutations: [ZeGraphMutation] = []
  var literalExpressions: [ZeGraphExpression] = []
  var originalExpressionCount: UInt32 = 0

  func names(_ strings: [String]) throws -> ZeGraphRange {
    let ranges = try strings.map(pool.string)
    let range = try pool.range(pool.names.count, ranges.count)
    pool.names.append(contentsOf: ranges)
    return range
  }
  func bindings(_ bindings: [GraphProjection]) throws -> ZeGraphRange {
    let range = try pool.range(projections.count, bindings.count)
    for binding in bindings {
      var p = ZeGraphProjection()
      p.abi_size = graphSize(ZeGraphProjection.self)
      p.slot = binding.slot.rawValue
      p.expression = binding.expression.rawValue
      projections.append(p)
    }
    return range
  }
  func expression(_ expression: GraphExpression, literalDepth: Int = 0) throws -> ZeGraphExpression
  {
    guard literalDepth <= 16 else {
      throw GraphError(.invalidRequest("literal list depth exceeds 16"))
    }
    var e = ZeGraphExpression()
    e.abi_size = graphSize(ZeGraphExpression.self)
    switch expression {
    case .literal(.list(let elements)):
      guard literalDepth < 16 else {
        throw GraphError(.invalidRequest("literal list depth exceeds 16"))
      }
      // The ABI Literal kind is scalar. Lists use expression children, with
      // supplemental arena entries so original public indices never shift.
      let indices = try elements.map { value -> UInt32 in
        let expression = try self.expression(.literal(value), literalDepth: literalDepth + 1)
        guard literalExpressions.count < 4096 else {
          throw GraphError(.invalidRequest("literal expression arena exceeds 4096"))
        }
        guard let offset = UInt32(exactly: literalExpressions.count),
          let index = UInt32(exactly: UInt64(originalExpressionCount) + UInt64(offset))
        else {
          throw GraphError(.invalidRequest("literal expression arena too large"))
        }
        literalExpressions.append(expression)
        return index
      }
      e.kind = 7
      e.children = try pool.range(children.count, indices.count)
      children.append(contentsOf: indices)
    case .literal(let v):
      e.kind = 0
      e.value = try pool.scalar(v)
    case .slot(let id):
      e.kind = 1
      e.value = id.rawValue
    case .parameter(let id):
      e.kind = 2
      e.value = id.rawValue
    case .unary(let op, let left):
      e.kind = 3
      e.operation = op.rawValue
      e.left = left.rawValue
    case .binary(let op, let left, let right):
      e.kind = 4
      e.operation = op.rawValue
      e.left = left.rawValue
      e.right = right.rawValue
    case .property(let left, let name):
      e.kind = 5
      e.left = left.rawValue
      e.name = try pool.string(name)
    case .hasLabel(let left, let name):
      e.kind = 6
      e.left = left.rawValue
      e.name = try pool.string(name)
    case .list(let ids):
      e.kind = 7
      e.children = try pool.range(children.count, ids.count)
      children.append(contentsOf: ids.map(\.rawValue))
    case .aggregate(let op, let operand, let distinct):
      e.kind = 8
      e.operation = op.rawValue
      e.has_operand = operand == nil ? 0 : 1
      e.left = operand?.rawValue ?? 0
      e.distinct = distinct ? 1 : 0
    }
    return e
  }
  func mutation(_ mutation: GraphMutation) throws -> ZeGraphMutation {
    var m = ZeGraphMutation()
    m.abi_size = graphSize(ZeGraphMutation.self)
    switch mutation {
    case .createNode(let output, let labels):
      m.kind = 0
      m.output = output.rawValue
      m.labels = try names(labels)
    case .createRelationship(let output, let source, let target, let type):
      m.kind = 1
      m.output = output.rawValue
      m.source = source.rawValue
      m.target = target.rawValue
      m.name = try pool.string(type)
    case .removeProperty(let entity, let name):
      m.kind = 2
      m.entity = entity.rawValue
      m.name = try pool.string(name)
    case .setLabel(let entity, let name, let present):
      m.kind = 3
      m.entity = entity.rawValue
      m.name = try pool.string(name)
      m.present = present ? 1 : 0
    case .delete(let entity, let detach):
      m.kind = 4
      m.entity = entity.rawValue
      m.detach = detach ? 1 : 0
    case .setProperty(let entity, let name, let value):
      m.kind = 5
      m.entity = entity.rawValue
      m.name = try pool.string(name)
      m.value = value.rawValue
    }
    return m
  }
  func expansion(_ expansion: GraphExpansion, into o: inout ZeGraphOperator) throws {
    o.source_slot = expansion.source.rawValue
    o.node_slot = expansion.node.rawValue
    o.relationship_slot = expansion.relationship.rawValue
    o.direction = expansion.direction.rawValue
    o.pattern = expansion.pattern.rawValue
    o.relationship_types = try names(expansion.types)
  }
  func operation(_ operation: GraphOperator) throws -> ZeGraphOperator {
    var o = ZeGraphOperator()
    o.abi_size = graphSize(ZeGraphOperator.self)
    var dependencies: [GraphOperatorID] = []
    switch operation {
    case .unit: o.kind = 0
    case .join(let left, let right, let predicate):
      o.kind = 1
      dependencies = [left, right]
      o.predicate = optional(predicate?.rawValue)
    case .distinct(let input):
      o.kind = 2
      dependencies = [input]
    case .sort(let input, let keys):
      o.kind = 3
      dependencies = [input]
      o.sort_keys = try pool.range(sorts.count, keys.count)
      for key in keys {
        var s = ZeGraphSortKey()
        s.abi_size = graphSize(ZeGraphSortKey.self)
        s.expression = key.expression.rawValue
        s.descending = key.descending ? 1 : 0
        sorts.append(s)
      }
    case .scanNodes(let output, let label):
      o.kind = 4
      o.node_slot = output.rawValue
      if let label {
        o.has_name = 1
        o.name = try pool.string(label)
      }
    case .eager(let input):
      o.kind = 5
      dependencies = [input]
    case .mutate(let input, let items):
      o.kind = 6
      dependencies = [input]
      let encoded = try items.map(mutation)
      o.mutations = try pool.range(mutations.count, encoded.count)
      mutations.append(contentsOf: encoded)
    case .aggregate(let input, let keys, let aggregates):
      o.kind = 7
      dependencies = [input]
      o.projections = try bindings(keys)
      o.aggregates = try bindings(aggregates)
    case .search(let id, let eligibility):
      o.kind = 8
      o.search = id.rawValue
      dependencies = eligibility.map { [$0] } ?? []
    case .offsetLimit(let input, let offset, let limit):
      o.kind = 9
      dependencies = [input]
      o.offset = offset
      o.has_limit = limit == nil ? 0 : 1
      o.limit = limit ?? 0
    case .lookupNode(let output, let id):
      o.kind = 10
      o.node_slot = output.rawValue
      o.node_id = ZeNodeId(high: id.high, low: id.low)
    case .lookupRelationship(let output, let id):
      o.kind = 11
      o.relationship_slot = output.rawValue
      o.relationship_id = ZeRelId(high: id.high, low: id.low)
    case .lookupKey(let output, let kind, let namespace, let key):
      o.kind = 12
      o.node_slot = output.rawValue
      o.entity_kind = kind.rawValue
      o.has_name = 1
      o.name = try pool.string(namespace)
      o.key_expression = key.rawValue
    case .expand(let input, let spec):
      o.kind = 13
      dependencies = [input]
      try expansion(spec, into: &o)
    case .boundedExpand(let input, let spec, let min, let max, let predicate):
      o.kind = 14
      dependencies = [input]
      try expansion(spec, into: &o)
      o.path_min = min
      o.path_max = max
      o.edge_predicate = optional(predicate?.expression.rawValue)
      o.edge_slot = predicate?.slot.rawValue ?? 0
    case .optionalApply(let left, let right, let predicate):
      o.kind = 15
      dependencies = [left, right]
      o.predicate = optional(predicate?.rawValue)
    case .project(let input, let bindings):
      o.kind = 16
      dependencies = [input]
      o.projections = try self.bindings(bindings)
    case .with(let input, let bindings):
      o.kind = 17
      dependencies = [input]
      o.projections = try self.bindings(bindings)
    case .filter(let input, let predicate):
      o.kind = 18
      dependencies = [input]
      o.predicate = optional(predicate.rawValue)
    case .collect(let input):
      o.kind = 19
      dependencies = [input]
    case .eligibleSet(let input, let source, let output):
      o.kind = 20
      dependencies = [input]
      o.source_slot = source.rawValue
      o.set_slot = output.rawValue
    }
    if !dependencies.isEmpty {
      o.inputs = try pool.range(inputs.count, dependencies.count)
      inputs.append(contentsOf: dependencies.map(\.rawValue))
    }
    return o
  }
  func searchOptions(_ options: GraphSearchOptions?) -> ZeGraphSearchOptions {
    var s = ZeGraphSearchOptions()
    s.abi_size = graphSize(ZeGraphSearchOptions.self)
    if let options {
      s.graph_profile = UInt32(options.profile.rawValue)
      s.graph_ef = options.ef
      s.graph_seed = options.seed
      s.lexical_flags = options.lastAsPrefix ? 1 : 0
      s.rescore = options.rescore.rawValue
      s.has_alpha = options.alpha == nil ? 0 : 1
      s.alpha = options.alpha ?? 0
      s.rules_enabled = options.rulesEnabled ? 1 : 0
      s.has_max_rounds = options.maxRounds == nil ? 0 : 1
      s.max_rounds = options.maxRounds ?? 0
    }
    return s
  }
  func search(_ search: GraphSearch) -> ZeGraphSearch {
    var s = ZeGraphSearch()
    s.abi_size = graphSize(ZeGraphSearch.self)
    switch search.kind {
    case .vector(let v):
      s.kind = 0
      s.vector = optional(v.rawValue)
    case .text(let t):
      s.kind = 1
      s.text = optional(t.rawValue)
    case .hybrid(let v, let t):
      s.kind = 2
      s.vector = optional(v.rawValue)
      s.text = optional(t.rawValue)
    }
    s.call_id = search.call.rawValue
    s.k = search.k.rawValue
    s.has_tier = search.tier == nil ? 0 : 1
    s.tier = search.tier?.rawValue ?? 0
    s.eligible_set = optional(search.eligibleSet?.rawValue)
    s.window = optional(search.window?.rawValue)
    s.node_slot = search.node.rawValue
    s.score_slot = search.score.rawValue
    s.vector_distance_slot = optional(search.vectorDistance?.rawValue)
    s.lexical_score_slot = optional(search.lexicalScore?.rawValue)
    return s
  }
  func withPlan<T>(_ plan: GraphPlan, _ body: (UnsafePointer<ZeGraphPlan>) throws -> T) throws -> T
  {
    let operators = try plan.operators.map(operation)
    guard let count = UInt32(exactly: plan.expressions.count) else {
      throw GraphError(.invalidRequest("expression arena too large"))
    }
    originalExpressionCount = count
    var expressions = try plan.expressions.map { try expression($0) }
    expressions.append(contentsOf: literalExpressions)
    let parameters = try plan.parameters.map { declaration -> ZeGraphParameter in
      var p = ZeGraphParameter()
      p.abi_size = graphSize(ZeGraphParameter.self)
      p.name = try pool.string(declaration.name)
      p.kinds = declaration.kinds.rawValue
      return p
    }
    let options = plan.searches.map { searchOptions($0.options) }
    let eager = plan.eagerSearches.map(\.rawValue)
    return try options.withUnsafeBufferPointer { options in
      var searches = plan.searches.map(search)
      for index in searches.indices where plan.searches[index].options != nil {
        searches[index].options = options.baseAddress?.advanced(by: index)
      }
      return try operators.withUnsafeBufferPointer { operators in
        try expressions.withUnsafeBufferPointer { expressions in
          try inputs.withUnsafeBufferPointer { inputs in
            try children.withUnsafeBufferPointer { children in
              try projections.withUnsafeBufferPointer { projections in
                try sorts.withUnsafeBufferPointer { sorts in
                  try mutations.withUnsafeBufferPointer { mutations in
                    try parameters.withUnsafeBufferPointer { parameters in
                      try searches.withUnsafeBufferPointer { searches in
                        try eager.withUnsafeBufferPointer { eager in
                          try pool.withPool { pool in
                            var pool = pool
                            return try withUnsafePointer(to: &pool) { pool in
                              var native = ZeGraphPlan()
                              native.abi_size = graphSize(ZeGraphPlan.self)
                              native.root = plan.root.rawValue
                              native.pool = pool
                              native.operators = operators.baseAddress
                              native.operator_count = operators.count
                              native.expressions = expressions.baseAddress
                              native.expression_count = expressions.count
                              native.inputs = inputs.baseAddress
                              native.input_count = inputs.count
                              native.expression_children = children.baseAddress
                              native.expression_child_count = children.count
                              native.projections = projections.baseAddress
                              native.projection_count = projections.count
                              native.sort_keys = sorts.baseAddress
                              native.sort_key_count = sorts.count
                              native.mutations = mutations.baseAddress
                              native.mutation_count = mutations.count
                              native.parameters = parameters.baseAddress
                              native.parameter_count = parameters.count
                              native.searches = searches.baseAddress
                              native.search_count = searches.count
                              native.eager_searches = eager.baseAddress
                              native.eager_search_count = eager.count
                              return try withUnsafePointer(to: &native, body)
                            }
                          }
                        }
                      }
                    }
                  }
                }
              }
            }
          }
        }
      }
    }
  }
}

#endif
