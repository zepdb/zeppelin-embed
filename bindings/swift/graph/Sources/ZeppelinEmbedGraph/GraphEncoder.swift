import CZeppelinEmbedGraph
import Foundation

// One synchronous borrow scope owns every request array. No pointer crosses await.
final class GraphEncoder {
  var bytes: [UInt8] = [], values: [ZeGraphValue] = [], children: [UInt32] = []
  var nodes: [ZeGraphNode] = [], relationships: [ZeGraphRelationship] = []
  var properties: [ZeGraphProperty] = [], names: [ZeGraphRange] = [], vectors: [Float] = []
  func range(_ start: Int, _ count: Int) throws -> ZeGraphRange {
    guard let s = UInt32(exactly: start), let c = UInt32(exactly: count) else {
      throw GraphError(.invalidRequest("pool too large"))
    }
    return ZeGraphRange(start: s, count: c)
  }
  func string(_ text: String) throws -> ZeGraphRange {
    let encoded = Array(text.utf8)
    let span = try range(bytes.count, encoded.count)
    bytes.append(contentsOf: encoded)
    return span
  }
  func scalar(_ parameter: GraphParameter) throws -> UInt32 {
    var v = ZeGraphValue()
    v.abi_size = graphSize(ZeGraphValue.self)
    switch parameter {
    case .null: v.tag = 0
    case .bool(let b):
      v.tag = 1
      v.boolean = b ? 1 : 0
    case .integer(let i):
      v.tag = 2
      v.integer = i
    case .double(let d):
      v.tag = 3
      v.floating = d
    case .string(let s):
      v.tag = 4
      v.range = try string(s)
    }
    guard let index = UInt32(exactly: values.count) else {
      throw GraphError(.invalidRequest("values too large"))
    }
    values.append(v)
    return index
  }
  func property(_ p: GraphProperty) throws -> UInt32 {
    var kind: GraphListKind
    var elements: [GraphParameter]
    switch p {
    case .bool(let v): return try scalar(.bool(v))
    case .integer(let v): return try scalar(.integer(v))
    case .double(let v): return try scalar(.double(v))
    case .string(let v): return try scalar(.string(v))
    case .bools(let a):
      kind = .bool
      elements = a.map(GraphParameter.bool)
    case .integers(let a):
      kind = .integer
      elements = a.map(GraphParameter.integer)
    case .doubles(let a):
      kind = .double
      elements = a.map(GraphParameter.double)
    case .strings(let a):
      kind = .string
      elements = a.map(GraphParameter.string)
    case .emptyList:
      kind = .empty
      elements = []
    }
    let indices = try elements.map(scalar)
    var v = ZeGraphValue()
    v.abi_size = graphSize(ZeGraphValue.self)
    v.tag = 7
    v.list_kind = kind.rawValue
    v.range = try range(children.count, indices.count)
    children.append(contentsOf: indices)
    guard let index = UInt32(exactly: values.count) else {
      throw GraphError(.invalidRequest("values too large"))
    }
    values.append(v)
    return index
  }
  func propertyMap(_ map: [String: GraphProperty]) throws -> ZeGraphRange {
    let start = properties.count
    for key in map.keys.sorted() {
      guard let value = map[key] else { continue }
      var p = ZeGraphProperty()
      p.abi_size = graphSize(ZeGraphProperty.self)
      p.name = try string(key)
      p.value = try property(value)
      properties.append(p)
    }
    return try range(start, properties.count - start)
  }
  func node(_ image: GraphNodeImage) throws -> UInt32 {
    var n = ZeGraphNode()
    n.abi_size = graphSize(ZeGraphNode.self)
    n.properties = try propertyMap(image.properties)
    let labelRanges = try image.labels.map(string)
    n.labels = try range(names.count, labelRanges.count)
    names.append(contentsOf: labelRanges)
    if let text = image.text {
      n.has_text = 1
      n.text = try string(text)
    }
    if let vector = image.vector {
      n.has_vector = 1
      n.vector = try range(vectors.count, vector.count)
      vectors.append(contentsOf: vector)
    }
    guard let index = UInt32(exactly: nodes.count) else {
      throw GraphError(.invalidRequest("nodes too large"))
    }
    nodes.append(n)
    return index
  }
  func relationship(_ image: GraphRelationshipImage) throws -> UInt32 {
    var r = ZeGraphRelationship()
    r.abi_size = graphSize(ZeGraphRelationship.self)
    r.relationship_type = try string(image.type)
    r.properties = try propertyMap(image.properties)
    guard let index = UInt32(exactly: relationships.count) else {
      throw GraphError(.invalidRequest("relationships too large"))
    }
    relationships.append(r)
    return index
  }
  func endpoint(_ endpoint: GraphEndpoint, batch: GraphBatch) throws -> ZeGraphEndpoint {
    var e = ZeGraphEndpoint()
    e.abi_size = graphSize(ZeGraphEndpoint.self)
    switch endpoint {
    case .node(let id):
      e.kind = 1
      e.node = ZeNodeId(high: id.high, low: id.low)
    case .local(let local):
      guard local.batch == batch.identity, Int(local.item) < batch.items.count,
        case .node(_, _, let mutation) = batch.items[Int(local.item)]
      else { throw GraphError(.invalidRequest("foreign local node")) }
      switch mutation {
      case .delete: throw GraphError(.invalidRequest("deleted local node"))
      default: break
      }
      e.kind = 2
      e.local_item = local.item
    }
    return e
  }
  func batch(_ batch: GraphBatch) throws -> [ZeGraphBatchItem] {
    guard batch.items.count <= 16384 else {
      throw GraphError(.invalidRequest("batch size"))
    }
    return try batch.items.map { item in
      var b = ZeGraphBatchItem()
      b.abi_size = graphSize(ZeGraphBatchItem.self)
      b.source.abi_size = graphSize(ZeGraphEndpoint.self)
      b.target.abi_size = graphSize(ZeGraphEndpoint.self)
      var key: GraphKey
      switch item {
      case .node(let k, let revision, let mutation):
        key = k
        b.revision = revision
        b.entity_kind = 0
        var image: GraphNodeImage?
        switch mutation {
        case .create(let i):
          b.operation = 0
          image = i
        case .put(let id, let i):
          b.operation = 1
          b.expected_node = ZeNodeId(high: id.high, low: id.low)
          image = i
        case .delete(let id, let detach):
          b.operation = 2
          b.expected_node = ZeNodeId(high: id.high, low: id.low)
          b.delete_mode = detach ? 1 : 0
        case .recreate(let revision, let i):
          b.operation = 3
          b.expected_deletion_revision = revision
          image = i
        }
        if let image {
          b.has_image = 1
          b.image = try node(image)
        }
      case .relationship(let k, let revision, let mutation):
        key = k
        b.revision = revision
        b.entity_kind = 1
        var image: GraphRelationshipImage?
        var source: GraphEndpoint?
        var target: GraphEndpoint?
        switch mutation {
        case .create(let i, let s, let t):
          b.operation = 0
          image = i
          source = s
          target = t
        case .put(let id, let i, let s, let t):
          b.operation = 1
          b.expected_relationship = ZeRelId(high: id.high, low: id.low)
          image = i
          source = s
          target = t
        case .delete(let id):
          b.operation = 2
          b.expected_relationship = ZeRelId(high: id.high, low: id.low)
        case .recreate(let revision, let i, let s, let t):
          b.operation = 3
          b.expected_deletion_revision = revision
          image = i
          source = s
          target = t
        }
        if let image {
          b.has_image = 1
          b.image = try relationship(image)
        }
        if let source, let target {
          b.source = try endpoint(source, batch: batch)
          b.target = try endpoint(target, batch: batch)
        }
      }
      b.namespace_name = try string(key.namespace)
      b.key = try string(key.key)
      return b
    }
  }
  func withPool<T>(_ body: (ZeGraphValuePool) throws -> T) rethrows -> T {
    try values.withUnsafeBufferPointer { v in
      try children.withUnsafeBufferPointer { c in
        try bytes.withUnsafeBufferPointer { b in
          try nodes.withUnsafeBufferPointer { n in
            try relationships.withUnsafeBufferPointer { r in
              try properties.withUnsafeBufferPointer { p in
                try names.withUnsafeBufferPointer { names in
                  try vectors.withUnsafeBufferPointer { vectors in
                    var pool = ZeGraphValuePool()
                    pool.abi_size = graphSize(ZeGraphValuePool.self)
                    pool.values = v.baseAddress
                    pool.value_count = v.count
                    pool.children = c.baseAddress
                    pool.child_count = c.count
                    pool.bytes = b.baseAddress
                    pool.byte_count = b.count
                    pool.nodes = n.baseAddress
                    pool.node_count = n.count
                    pool.relationships = r.baseAddress
                    pool.relationship_count = r.count
                    pool.properties = p.baseAddress
                    pool.property_count = p.count
                    pool.names = names.baseAddress
                    pool.name_count = names.count
                    pool.vectors = vectors.baseAddress
                    pool.vector_count = vectors.count
                    return try body(pool)
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
