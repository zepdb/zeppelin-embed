#if ZE_GRAPH
import Foundation

// Arena indices and logical IDs remain distinct. Native admission validates
// references, types, scope and capabilities before execution.
public struct GraphOperatorID: Sendable, Hashable {
  public let rawValue: UInt32
  public init(_ value: UInt32) { rawValue = value }
}
public struct GraphExpressionID: Sendable, Hashable {
  public let rawValue: UInt32
  public init(_ value: UInt32) { rawValue = value }
}
public struct GraphSlotID: Sendable, Hashable {
  public let rawValue: UInt32
  public init(_ value: UInt32) { rawValue = value }
}
public struct GraphParameterID: Sendable, Hashable {
  public let rawValue: UInt32
  public init(_ value: UInt32) { rawValue = value }
}
public struct GraphSearchID: Sendable, Hashable {
  public let rawValue: UInt32
  public init(_ value: UInt32) { rawValue = value }
}
public struct GraphSearchCallID: Sendable, Hashable {
  public let rawValue: UInt32
  public init(_ value: UInt32) { rawValue = value }
}
public struct GraphEligibleSetID: Sendable, Hashable {
  public let rawValue: UInt32
  public init(_ value: UInt32) { rawValue = value }
}
public struct GraphPatternID: Sendable, Hashable {
  public let rawValue: UInt32
  public init(_ value: UInt32) { rawValue = value }
}

public struct GraphParameterKinds: OptionSet, Sendable {
  public let rawValue: UInt32
  public init(rawValue: UInt32) { self.rawValue = rawValue }
  public static let null = Self(rawValue: 1), bool = Self(rawValue: 2), integer = Self(rawValue: 4),
    double = Self(rawValue: 8), string = Self(rawValue: 16), list = Self(rawValue: 128)
  public static let nonentity: Self = [.null, .bool, .integer, .double, .string, .list]
}
public struct GraphParameterDeclaration: Sendable {
  public let name: String
  public let kinds: GraphParameterKinds
  public init(_ name: String, kinds: GraphParameterKinds) {
    self.name = name
    self.kinds = kinds
  }
}
public enum GraphUnaryOperation: UInt32, Sendable {
  case not, positive, negate, isNull, isNotNull, size, labels, relationshipType, storedText,
    nodeIDText, relationshipIDText
}
public enum GraphBinaryOperation: UInt32, Sendable {
  case and, or, xor, equal, notEqual, less, lessEqual, greater, greaterEqual, add, subtract,
    multiply, divide, remainder, startsWith, endsWith, contains, inList, index
}
public enum GraphAggregateOperation: UInt32, Sendable { case count, collect }
public enum GraphExpression: Sendable {
  case literal(GraphParameter)
  case slot(GraphSlotID)
  case parameter(GraphParameterID)
  case unary(GraphUnaryOperation, GraphExpressionID)
  case binary(GraphBinaryOperation, GraphExpressionID, GraphExpressionID)
  case property(GraphExpressionID, String)
  case hasLabel(GraphExpressionID, String)
  case list([GraphExpressionID])
  case aggregate(GraphAggregateOperation, operand: GraphExpressionID?, distinct: Bool)
}
public struct GraphProjection: Sendable {
  public let slot: GraphSlotID
  public let expression: GraphExpressionID
  public init(_ slot: GraphSlotID, _ expression: GraphExpressionID) {
    self.slot = slot
    self.expression = expression
  }
}
public struct GraphSortKey: Sendable {
  public let expression: GraphExpressionID
  public let descending: Bool
  public init(_ expression: GraphExpressionID, descending: Bool = false) {
    self.expression = expression
    self.descending = descending
  }
}
public enum GraphEntityKind: UInt32, Sendable { case node, relationship }
public enum GraphDirection: UInt32, Sendable { case outgoing, incoming, either }
public struct GraphExpansion: Sendable {
  public var source: GraphSlotID, node: GraphSlotID, relationship: GraphSlotID
  public var direction: GraphDirection
  public var types: [String]
  public var pattern: GraphPatternID
  public init(
    source: GraphSlotID, node: GraphSlotID, relationship: GraphSlotID,
    direction: GraphDirection = .outgoing, types: [String] = [],
    pattern: GraphPatternID = GraphPatternID(0)
  ) {
    self.source = source
    self.node = node
    self.relationship = relationship
    self.direction = direction
    self.types = types
    self.pattern = pattern
  }
}
public struct GraphEdgePredicate: Sendable {
  public let slot: GraphSlotID
  public let expression: GraphExpressionID
  public init(slot: GraphSlotID, expression: GraphExpressionID) {
    self.slot = slot
    self.expression = expression
  }
}
public enum GraphMutation: Sendable {
  case createNode(output: GraphSlotID, labels: [String])
  case createRelationship(
    output: GraphSlotID, source: GraphExpressionID, target: GraphExpressionID, type: String)
  case removeProperty(entity: GraphExpressionID, name: String)
  case setLabel(entity: GraphExpressionID, name: String, present: Bool)
  case delete(entity: GraphExpressionID, detach: Bool)
  case setProperty(entity: GraphExpressionID, name: String, value: GraphExpressionID)
}
public enum GraphOperator: Sendable {
  case unit
  case join(left: GraphOperatorID, right: GraphOperatorID, predicate: GraphExpressionID?)
  case distinct(input: GraphOperatorID)
  case sort(input: GraphOperatorID, keys: [GraphSortKey])
  case scanNodes(output: GraphSlotID, label: String?)
  case eager(input: GraphOperatorID)
  case mutate(input: GraphOperatorID, mutations: [GraphMutation])
  case aggregate(input: GraphOperatorID, keys: [GraphProjection], aggregates: [GraphProjection])
  case search(GraphSearchID, eligibility: GraphOperatorID?)
  case offsetLimit(input: GraphOperatorID, offset: UInt64, limit: UInt64?)
  case lookupNode(output: GraphSlotID, id: GraphNodeID)
  case lookupRelationship(output: GraphSlotID, id: GraphRelationshipID)
  case lookupKey(
    output: GraphSlotID, kind: GraphEntityKind, namespace: String, key: GraphExpressionID)
  case expand(input: GraphOperatorID, GraphExpansion)
  case boundedExpand(
    input: GraphOperatorID, GraphExpansion, min: UInt32, max: UInt32,
    edgePredicate: GraphEdgePredicate?)
  case optionalApply(left: GraphOperatorID, right: GraphOperatorID, predicate: GraphExpressionID?)
  case project(input: GraphOperatorID, bindings: [GraphProjection])
  case with(input: GraphOperatorID, bindings: [GraphProjection])
  case filter(input: GraphOperatorID, predicate: GraphExpressionID)
  case collect(input: GraphOperatorID)
  case eligibleSet(input: GraphOperatorID, source: GraphSlotID, output: GraphEligibleSetID)
}
public enum GraphTier: UInt32, Sendable { case auto, exact, scan, graph }
public enum GraphSearchKind: Sendable {
  case vector(GraphExpressionID)
  case text(GraphExpressionID)
  case hybrid(vector: GraphExpressionID, text: GraphExpressionID)
}
public enum GraphRescore: UInt32, Sendable { case none, originalFloat32 }
public struct GraphSearchOptions: Sendable {
  public var profile: GraphProfile = .sift
  public var ef: UInt32 = 0
  public var seed: UInt64 = 0
  public var lastAsPrefix = false
  public var rescore: GraphRescore = .none
  public var alpha: Double?
  public var rulesEnabled = false
  public var maxRounds: UInt64?
  public init() {}
}
public struct GraphSearch: Sendable {
  public var kind: GraphSearchKind
  public var call: GraphSearchCallID
  public var k: GraphExpressionID
  public var node: GraphSlotID, score: GraphSlotID
  public var tier: GraphTier?
  public var eligibleSet: GraphEligibleSetID?
  public var window: GraphExpressionID?
  public var vectorDistance: GraphSlotID?, lexicalScore: GraphSlotID?
  public var options: GraphSearchOptions?
  public init(
    _ kind: GraphSearchKind, call: GraphSearchCallID, k: GraphExpressionID,
    node: GraphSlotID, score: GraphSlotID, tier: GraphTier? = nil,
    eligibleSet: GraphEligibleSetID? = nil, window: GraphExpressionID? = nil,
    vectorDistance: GraphSlotID? = nil, lexicalScore: GraphSlotID? = nil,
    options: GraphSearchOptions? = nil
  ) {
    self.kind = kind
    self.call = call
    self.k = k
    self.node = node
    self.score = score
    self.tier = tier
    self.eligibleSet = eligibleSet
    self.window = window
    self.vectorDistance = vectorDistance
    self.lexicalScore = lexicalScore
    self.options = options
  }
}
public struct GraphPlan: Sendable {
  public var root: GraphOperatorID
  public var operators: [GraphOperator]
  public var expressions: [GraphExpression]
  public var parameters: [GraphParameterDeclaration]
  public var searches: [GraphSearch]
  // Operator indices in source order, including searches outside the root DAG.
  public var eagerSearches: [GraphOperatorID]
  public init(
    root: GraphOperatorID, operators: [GraphOperator], expressions: [GraphExpression] = [],
    parameters: [GraphParameterDeclaration] = [], searches: [GraphSearch] = [],
    eagerSearches: [GraphOperatorID] = []
  ) {
    self.root = root
    self.operators = operators
    self.expressions = expressions
    self.parameters = parameters
    self.searches = searches
    self.eagerSearches = eagerSearches
  }
}

#endif
