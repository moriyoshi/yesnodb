package yesnodb

import (
	"encoding/binary"
	"errors"
	"fmt"
	"math"
	"slices"
)

const (
	// MaxExpressionDepth bounds recursive decoding of untrusted descriptors.
	MaxExpressionDepth = 32
	// MaxExpressionNodes bounds allocation from untrusted junction counts.
	MaxExpressionNodes = 4096
	// MaxViewSets bounds a view descriptor's constituent count. Neither of the
	// bounds above sees it: a descriptor is a fixed 13 bytes whatever it
	// declares, while the server loops over every constituent.
	MaxViewSets = 4096
)

const (
	expressionVersion byte = 1
	expressionHeader       = 6
	queryVersion      byte = 1
	queryHeader            = 14
	queryFlagPinned   byte = 1
)

var (
	expressionMagic = [4]byte{'Y', 'S', 'N', 'X'}
	queryMagic      = [4]byte{'Y', 'S', 'N', 'Q'}
)

const (
	tagEmpty byte = iota
	tagKey
	tagRange
	tagAnd
	tagOr
	tagAndNot
	tagAt
	tagFold
	tagExpand
	tagLiteral
	tagPack
	// Vector-sorted nodes share one tag space with the set-sorted ones above,
	// so a node in the wrong position is a sort error rather than a
	// reinterpretation of whatever that byte means where it landed.
	tagList
	tagView
	tagHole
	tagSelect
	tagMapSet
	tagMapInt
	tagMapBool
	tagCardinality
	tagRank
	tagIntLit
	tagIntAt
	tagIntList
	tagContains
)

// Sort names, used in sort-mismatch errors.
const (
	sortSet    = "set"
	sortVecSet = "vector of sets"
	sortInt    = "integer"
	sortVecInt = "vector of integers"
	sortBool   = "boolean"
)

// sortOfTag is one table consulted by every decoder's fallback.
//
// Enumerating other sorts' tags inside each arm is how a tag ends up reported
// as unknown in one position and as a sort mismatch in another.
func sortOfTag(tag byte) (string, bool) {
	switch tag {
	case tagEmpty, tagKey, tagRange, tagAnd, tagOr, tagAndNot, tagAt, tagFold,
		tagExpand, tagLiteral, tagPack, tagHole, tagSelect, tagMapBool:
		return sortSet, true
	case tagList, tagView, tagMapSet:
		return sortVecSet, true
	case tagMapInt, tagIntList:
		return sortVecInt, true
	case tagCardinality, tagRank, tagIntLit, tagIntAt:
		return sortInt, true
	case tagContains:
		return sortBool, true
	}
	return "", false
}

// misplaced reports a known tag of another sort as a sort mismatch, and an
// undefined one as unknown.
func misplaced(expected string, tag byte) error {
	if _, ok := sortOfTag(tag); ok {
		return fmt.Errorf("tag %d where a %s was required", tag, expected)
	}
	return fmt.Errorf("unknown expression tag %d", tag)
}

// Expr is one remotely executable yesnodb set expression.
//
// The interface is sealed so every implementation has a defined wire tag.
type Expr interface {
	yesnoExpr()
}

// EmptyExpr matches no ordinals.
type EmptyExpr struct{}

func (EmptyExpr) yesnoExpr() {}

// KeyExpr reads one complete posting list.
type KeyExpr struct{ Key uint64 }

func (KeyExpr) yesnoExpr() {}

// RangeExpr matches the half-open ordinal range [Lo, Hi).
type RangeExpr struct{ Lo, Hi uint64 }

func (RangeExpr) yesnoExpr() {}

// LiteralExpr is a canonical materialized set of ordinals.
type LiteralExpr struct{ Ordinals []uint64 }

func (LiteralExpr) yesnoExpr() {}

// AndExpr intersects one or more operands.
type AndExpr struct{ Operands []Expr }

func (AndExpr) yesnoExpr() {}

// OrExpr unions one or more operands.
type OrExpr struct{ Operands []Expr }

func (OrExpr) yesnoExpr() {}

// AndNotExpr subtracts Exclude from Include.
type AndNotExpr struct{ Include, Exclude Expr }

func (AndNotExpr) yesnoExpr() {}

// ViewLayout describes how logical view constituents share physical ordinals.
type ViewLayout uint8

const (
	// ViewInterleaved stores logical ordinal x of set i at x*sets+i.
	ViewInterleaved ViewLayout = iota
	// ViewBlocked stores logical ordinal x of set i at i*stride+x.
	ViewBlocked
)

// ViewSpec is caller-owned metadata for interpreting one packed key.
type ViewSpec struct {
	Sets   uint32
	Layout ViewLayout
	Stride uint64
}

// InterleavedView constructs a canonical interleaved view descriptor.
func InterleavedView(sets uint32) ViewSpec {
	return ViewSpec{Sets: sets, Layout: ViewInterleaved}
}

// BlockedView constructs a blocked view descriptor.
func BlockedView(sets uint32, stride uint64) ViewSpec {
	return ViewSpec{Sets: sets, Layout: ViewBlocked, Stride: stride}
}

// Validate checks that a view descriptor is canonical and useful.
func (v ViewSpec) Validate() error {
	if v.Sets == 0 {
		return errors.New("view packs zero constituents")
	}
	if v.Sets > MaxViewSets {
		return fmt.Errorf("view packs more than %d constituents", MaxViewSets)
	}
	switch v.Layout {
	case ViewInterleaved:
		if v.Stride != 0 {
			return errors.New("interleaved view carries a non-zero stride")
		}
	case ViewBlocked:
		if v.Stride == 0 {
			return errors.New("blocked view stride is zero")
		}
	default:
		return fmt.Errorf("unknown view layout %d", v.Layout)
	}
	return nil
}

// OrdinalOf returns the physical ordinal for one logical view position.
func (v ViewSpec) OrdinalOf(set uint32, logical uint64) (uint64, bool) {
	if v.Validate() != nil || set >= v.Sets {
		return 0, false
	}
	var ordinal uint64
	switch v.Layout {
	case ViewInterleaved:
		if logical > (math.MaxUint64-uint64(set))/uint64(v.Sets) {
			return 0, false
		}
		ordinal = logical*uint64(v.Sets) + uint64(set)
	case ViewBlocked:
		if logical >= v.Stride || uint64(set) > (math.MaxUint64-logical)/v.Stride {
			return 0, false
		}
		ordinal = uint64(set)*v.Stride + logical
	}
	return ordinal, ordinal != math.MaxUint64
}

// ViewReduce selects a reduction across every packed constituent.
// FoldOp is how FoldExpr combines a vector's elements.
//
// Exactly three, and provably so: on {0,1} an associative, unital operation is
// automatically commutative and is one of four -- or, xor with unit 0 and and,
// iff with unit 1. iff keeps a fibre of zeros, so its result is dense however
// sparse the operand, and andnot is neither associative nor commutative.
type FoldOp uint8

const (
	FoldOr FoldOp = iota
	FoldAnd
	FoldXor
)

// VecExpr is a fixed-arity vector of sets -- what a view produces.
//
// A separate sealed interface from Expr so that an ill-sorted tree does not
// typecheck, leaving the decoder to check only what arrives as bytes. Vectors
// do not nest: the element sort is a set, never another vector.
type VecExpr interface {
	yesnoVecExpr()
	// Arity reports the element count, which is always statically known and is
	// what makes indexing total.
	Arity() uint32
}

// ListExpr is a literal vector, [ a, b, c ].
type ListExpr struct{ Elements []Expr }

func (ListExpr) yesnoVecExpr() {}

// Arity reports the literal element count.
func (l ListExpr) Arity() uint32 { return uint32(len(l.Elements)) }

// ViewExpr reads one set as View.Sets constituents -- the curry direction.
type ViewExpr struct {
	Input Expr
	View  ViewSpec
}

func (ViewExpr) yesnoVecExpr() {}

// Arity reports the descriptor's constituent count.
func (v ViewExpr) Arity() uint32 { return v.View.Sets }

// AtExpr is one element of a vector, by zero-based index -- v[ i ].
type AtExpr struct {
	Vector VecExpr
	Index  uint32
}

func (AtExpr) yesnoExpr() {}

// FoldExpr combines every element of a vector into one set.
type FoldExpr struct {
	Vector VecExpr
	Op     FoldOp
}

func (FoldExpr) yesnoExpr() {}

// PackExpr packs a vector's elements into one set -- the uncurry direction.
type PackExpr struct {
	Vector VecExpr
	View   ViewSpec
}

func (PackExpr) yesnoExpr() {}

// ExpandExpr maps every logical input ordinal into every constituent.
type ExpandExpr struct {
	Input Expr
	View  ViewSpec
}

func (ExpandExpr) yesnoExpr() {}

// HoleExpr is `_`, the element of the enclosing map body.
//
// The language has no variables, so this is a hole rather than a name: no
// binder, no scope, no closure. Every `_` in one body denotes the same element.
type HoleExpr struct{}

func (HoleExpr) yesnoExpr() {}

// SelectExpr is the n-th smallest ordinal, as a singleton -- or empty if there
// is none. It yields a set rather than an integer because it is genuinely
// partial, and an empty set says so without a sentinel a caller could forget.
type SelectExpr struct {
	Input Expr
	Index uint64
}

func (SelectExpr) yesnoExpr() {}

// MapSetExpr applies a set-valued query to every element of a vector.
type MapSetExpr struct {
	Vector VecExpr
	Body   Expr
}

func (MapSetExpr) yesnoVecExpr() {}

// Arity reports the operand's arity: a map preserves shape.
func (m MapSetExpr) Arity() uint32 { return m.Vector.Arity() }

// MapBoolExpr asks which constituents satisfy a boolean body.
//
// One truth value per constituent is a subset of the constituent indices, so
// this denotes a set rather than a vector sort.
type MapBoolExpr struct {
	Vector VecExpr
	Body   BoolExpr
}

func (MapBoolExpr) yesnoExpr() {}

// IntExpr is a single count or position.
type IntExpr interface{ yesnoIntExpr() }

// BoolExpr is a single truth value. Only ever a map body, never a query result.
type BoolExpr interface{ yesnoBoolExpr() }

// VecIntExpr is one integer per constituent.
//
// Always exactly the vector's arity long: a reduction indexed by ordinal must
// stay sparse, so only one indexed by constituent may be dense, which is what
// makes this sort unambiguous about which axis was collapsed.
type VecIntExpr interface {
	yesnoVecIntExpr()
	Arity() uint32
}

// IntLitExpr is a literal.
type IntLitExpr struct{ Value uint64 }

func (IntLitExpr) yesnoIntExpr() {}

// CardinalityExpr is how many ordinals a set holds.
type CardinalityExpr struct{ Input Expr }

func (CardinalityExpr) yesnoIntExpr() {}

// RankExpr is how many of a set's ordinals are strictly below a position.
type RankExpr struct {
	Input    Expr
	Position uint64
}

func (RankExpr) yesnoIntExpr() {}

// IntAtExpr is one element of a vector of integers, by zero-based index.
type IntAtExpr struct {
	Vector VecIntExpr
	Index  uint32
}

func (IntAtExpr) yesnoIntExpr() {}

// ContainsExpr is whether a set holds one ordinal.
type ContainsExpr struct {
	Input   Expr
	Ordinal uint64
}

func (ContainsExpr) yesnoBoolExpr() {}

// IntListExpr is a literal vector of integers.
type IntListExpr struct{ Elements []IntExpr }

func (IntListExpr) yesnoVecIntExpr() {}

// Arity reports the literal element count.
func (l IntListExpr) Arity() uint32 { return uint32(len(l.Elements)) }

// MapIntExpr applies a scalar query to every element -- the facet histogram.
//
// This is a map, not a fold: it does not combine the elements, it applies a
// query to each. The two are the marginals of the same matrix and do not
// determine each other.
type MapIntExpr struct {
	Vector VecExpr
	Body   IntExpr
}

func (MapIntExpr) yesnoVecIntExpr() {}

// Arity reports the operand's arity: a map preserves shape.
func (m MapIntExpr) Arity() uint32 { return m.Vector.Arity() }

// Empty constructs an empty expression.
func Empty() Expr { return EmptyExpr{} }

// Key constructs a whole-key expression.
func Key(key uint64) Expr { return KeyExpr{Key: key} }

// Range constructs a half-open ordinal range.
func Range(lo, hi uint64) Expr { return RangeExpr{Lo: lo, Hi: hi} }

// Literal constructs a canonical materialized ordinal set.
func Literal(ordinals ...uint64) (Expr, error) {
	canonical := append([]uint64(nil), ordinals...)
	if slices.Contains(canonical, uint64(math.MaxUint64)) {
		return nil, errors.New("18446744073709551615 is outside the ordinal universe")
	}
	slices.Sort(canonical)
	canonical = slices.Compact(canonical)
	return LiteralExpr{Ordinals: canonical}, nil
}

// And constructs an intersection. Encoding rejects an empty operand list.
func And(operands ...Expr) Expr {
	return AndExpr{Operands: append([]Expr(nil), operands...)}
}

// Or constructs a union. Encoding rejects an empty operand list.
func Or(operands ...Expr) Expr {
	return OrExpr{Operands: append([]Expr(nil), operands...)}
}

// AndNot constructs a set difference.
func AndNot(include, exclude Expr) Expr {
	return AndNotExpr{Include: include, Exclude: exclude}
}

// Xor derives symmetric difference using only Flight v1 nodes.
func Xor(left, right Expr) Expr {
	return AndNot(Or(left, right), And(left, right))
}

// Complement derives complement over [0, math.MaxUint64) using v1 nodes.
func Complement(input Expr) Expr {
	return AndNot(Range(0, math.MaxUint64), input)
}

// List constructs a literal vector of sets.
func List(elements ...Expr) VecExpr {
	return ListExpr{Elements: append([]Expr(nil), elements...)}
}

// View reads one set as view.Sets constituents.
func View(input Expr, view ViewSpec) VecExpr {
	return ViewExpr{Input: input, View: view}
}

// At selects one element of a vector by zero-based index.
func At(vector VecExpr, index uint32) Expr {
	return AtExpr{Vector: vector, Index: index}
}

// Fold combines every element of a vector into one set.
func Fold(vector VecExpr, op FoldOp) Expr {
	return FoldExpr{Vector: vector, Op: op}
}

// Pack packs a vector's elements into one set.
func Pack(vector VecExpr, view ViewSpec) Expr {
	return PackExpr{Vector: vector, View: view}
}

// Expand maps every logical input ordinal into every constituent.
func Expand(input Expr, view ViewSpec) Expr {
	return ExpandExpr{Input: input, View: view}
}

// Hole constructs `_`, the element of the enclosing map body.
func Hole() Expr { return HoleExpr{} }

// Select constructs the n-th smallest ordinal as a singleton set.
func Select(input Expr, index uint64) Expr {
	return SelectExpr{Input: input, Index: index}
}

// MapSet applies a set-valued query to every element of a vector.
func MapSet(vector VecExpr, body Expr) VecExpr {
	return MapSetExpr{Vector: vector, Body: body}
}

// MapBool asks which constituents satisfy a boolean body.
func MapBool(vector VecExpr, body BoolExpr) Expr {
	return MapBoolExpr{Vector: vector, Body: body}
}

// MapInt applies a scalar query to every element -- the facet histogram.
func MapInt(vector VecExpr, body IntExpr) VecIntExpr {
	return MapIntExpr{Vector: vector, Body: body}
}

// IntLit constructs an integer literal.
func IntLit(value uint64) IntExpr { return IntLitExpr{Value: value} }

// Cardinality constructs a set's cardinality.
func Cardinality(input Expr) IntExpr { return CardinalityExpr{Input: input} }

// Rank constructs a set's rank at a position.
func Rank(input Expr, position uint64) IntExpr {
	return RankExpr{Input: input, Position: position}
}

// IntAt indexes a vector of integers by zero-based position.
func IntAt(vector VecIntExpr, index uint32) IntExpr {
	return IntAtExpr{Vector: vector, Index: index}
}

// IntList constructs a literal vector of integers.
func IntList(elements ...IntExpr) VecIntExpr {
	return IntListExpr{Elements: append([]IntExpr(nil), elements...)}
}

// Contains constructs a membership test.
func Contains(input Expr, ordinal uint64) BoolExpr {
	return ContainsExpr{Input: input, Ordinal: ordinal}
}

// EncodeExpression returns the complete YSNX v1 descriptor command.
func EncodeExpression(expression Expr) ([]byte, error) {
	if err := validateExpression(expression); err != nil {
		return nil, err
	}
	out := make([]byte, 0, 32)
	out = append(out, expressionMagic[:]...)
	out = append(out, expressionVersion, 0)
	out = appendExpression(out, expression)
	if len(out) == 8 {
		return nil, errors.New("expression encoding is ambiguous with a bare key")
	}
	return out, nil
}

// EncodeIntVector returns the complete YSNX command for a query that denotes
// one integer per constituent -- a facet histogram.
//
// A query is usually a set. These are the only two shapes a server can return;
// a vector of *sets* is not among them, because it has no single answer shape
// and always reaches a result through a fold, a pack, or an index.
func EncodeIntVector(vector VecIntExpr) ([]byte, error) {
	if err := validateIntVector(vector); err != nil {
		return nil, err
	}
	out := make([]byte, 0, 32)
	out = append(out, expressionMagic[:]...)
	out = append(out, expressionVersion, 0)
	out = appendIntVector(out, vector)
	if len(out) == 8 {
		return nil, errors.New("expression encoding is ambiguous with a bare key")
	}
	return out, nil
}

// DecodeIntVector decodes one complete YSNX command at the integer-vector sort.
func DecodeIntVector(payload []byte) (VecIntExpr, error) {
	if len(payload) < expressionHeader {
		return nil, errors.New("expression ended in its header")
	}
	if string(payload[:4]) != string(expressionMagic[:]) {
		return nil, errors.New("not a yesnodb expression")
	}
	if payload[4] != expressionVersion || payload[5] != 0 {
		return nil, fmt.Errorf("unsupported expression version %d", payload[4])
	}
	c := &expressionCursor{payload: payload[expressionHeader:]}
	vector, err := c.intVector(0)
	if err != nil {
		return nil, err
	}
	if c.offset != len(c.payload) {
		return nil, errors.New("trailing bytes after expression")
	}
	return vector, nil
}

// walkers are the shape-checking entry points for each sort, sharing one node
// budget so a mixed-sort tree cannot exceed it by splitting across sorts.
type walkers struct {
	expr   func(Expr, int) error
	intVec func(VecIntExpr, int) error
}

func validateExpression(root Expr) error {
	return validate(func(w walkers) error { return w.expr(root, 0) })
}

func validateIntVector(root VecIntExpr) error {
	return validate(func(w walkers) error { return w.intVec(root, 0) })
}

func validate(start func(walkers) error) error {
	nodes := 0
	var visit func(Expr, int) error
	var visitVec func(VecExpr, int) error
	var visitBool func(BoolExpr, int) error
	var visitInt func(IntExpr, int) error
	var visitIntVec func(VecIntExpr, int) error
	visitVec = func(vector VecExpr, depth int) error {
		if vector == nil {
			return errors.New("expression contains a nil vector")
		}
		if depth > MaxExpressionDepth {
			return fmt.Errorf("expression nested deeper than %d", MaxExpressionDepth)
		}
		nodes++
		if nodes > MaxExpressionNodes {
			return fmt.Errorf("expression has more than %d nodes", MaxExpressionNodes)
		}
		switch v := vector.(type) {
		case ListExpr:
			if len(v.Elements) == 0 {
				return errors.New("vector has no elements")
			}
			if len(v.Elements) > math.MaxUint16 {
				return errors.New("vector has more than 65535 elements")
			}
			for _, element := range v.Elements {
				if err := visit(element, depth+1); err != nil {
					return err
				}
			}
		case ViewExpr:
			if err := v.View.Validate(); err != nil {
				return err
			}
			return visit(v.Input, depth+1)
		case MapSetExpr:
			if err := visitVec(v.Vector, depth+1); err != nil {
				return err
			}
			return visit(v.Body, depth+1)
		default:
			return fmt.Errorf("unsupported vector type %T", vector)
		}
		return nil
	}
	visitBool = func(expression BoolExpr, depth int) error {
		if depth > MaxExpressionDepth {
			return fmt.Errorf("expression nested deeper than %d", MaxExpressionDepth)
		}
		nodes++
		if nodes > MaxExpressionNodes {
			return fmt.Errorf("expression has more than %d nodes", MaxExpressionNodes)
		}
		e, ok := expression.(ContainsExpr)
		if !ok {
			return fmt.Errorf("unsupported boolean type %T", expression)
		}
		return visit(e.Input, depth+1)
	}
	visitInt = func(expression IntExpr, depth int) error {
		if depth > MaxExpressionDepth {
			return fmt.Errorf("expression nested deeper than %d", MaxExpressionDepth)
		}
		nodes++
		if nodes > MaxExpressionNodes {
			return fmt.Errorf("expression has more than %d nodes", MaxExpressionNodes)
		}
		switch e := expression.(type) {
		case IntLitExpr:
			return nil
		case CardinalityExpr:
			return visit(e.Input, depth+1)
		case RankExpr:
			return visit(e.Input, depth+1)
		case IntAtExpr:
			if err := visitIntVec(e.Vector, depth+1); err != nil {
				return err
			}
			if e.Index >= e.Vector.Arity() {
				return errors.New("index is at or above the vector's arity")
			}
			return nil
		default:
			return fmt.Errorf("unsupported integer type %T", expression)
		}
	}
	visitIntVec = func(vector VecIntExpr, depth int) error {
		if vector == nil {
			return errors.New("expression contains a nil vector")
		}
		if depth > MaxExpressionDepth {
			return fmt.Errorf("expression nested deeper than %d", MaxExpressionDepth)
		}
		nodes++
		if nodes > MaxExpressionNodes {
			return fmt.Errorf("expression has more than %d nodes", MaxExpressionNodes)
		}
		switch v := vector.(type) {
		case IntListExpr:
			if len(v.Elements) == 0 {
				return errors.New("vector has no elements")
			}
			if len(v.Elements) > math.MaxUint16 {
				return errors.New("vector has more than 65535 elements")
			}
			for _, element := range v.Elements {
				if err := visitInt(element, depth+1); err != nil {
					return err
				}
			}
		case MapIntExpr:
			if err := visitVec(v.Vector, depth+1); err != nil {
				return err
			}
			return visitInt(v.Body, depth+1)
		default:
			return fmt.Errorf("unsupported vector type %T", vector)
		}
		return nil
	}
	visit = func(expression Expr, depth int) error {
		if expression == nil {
			return errors.New("expression contains a nil node")
		}
		if depth > MaxExpressionDepth {
			return fmt.Errorf("expression nested deeper than %d", MaxExpressionDepth)
		}
		nodes++
		if nodes > MaxExpressionNodes {
			return fmt.Errorf("expression has more than %d nodes", MaxExpressionNodes)
		}
		switch e := expression.(type) {
		case EmptyExpr, KeyExpr, RangeExpr:
			return nil
		case LiteralExpr:
			if uint64(len(e.Ordinals)) > uint64(math.MaxUint32) {
				return errors.New("ordinal-set literal has more than 4294967295 members")
			}
			for index, ordinal := range e.Ordinals {
				if ordinal == math.MaxUint64 {
					return errors.New("18446744073709551615 is outside the ordinal universe")
				}
				if index > 0 && e.Ordinals[index-1] >= ordinal {
					return errors.New("ordinal-set literal is not strictly ascending and unique")
				}
			}
			return nil
		case AndExpr:
			if len(e.Operands) == 0 {
				return errors.New("AND has no operands")
			}
			for _, child := range e.Operands {
				if err := visit(child, depth+1); err != nil {
					return err
				}
			}
		case OrExpr:
			if len(e.Operands) == 0 {
				return errors.New("OR has no operands")
			}
			for _, child := range e.Operands {
				if err := visit(child, depth+1); err != nil {
					return err
				}
			}
		case AndNotExpr:
			if err := visit(e.Include, depth+1); err != nil {
				return err
			}
			return visit(e.Exclude, depth+1)
		case AtExpr:
			if err := visitVec(e.Vector, depth+1); err != nil {
				return err
			}
			if e.Index >= e.Vector.Arity() {
				return errors.New("index is at or above the vector's arity")
			}
		case FoldExpr:
			if err := visitVec(e.Vector, depth+1); err != nil {
				return err
			}
			if e.Op > FoldXor {
				return fmt.Errorf("unknown fold operator %d", e.Op)
			}
		case PackExpr:
			if err := e.View.Validate(); err != nil {
				return err
			}
			if err := visitVec(e.Vector, depth+1); err != nil {
				return err
			}
			if e.Vector.Arity() != e.View.Sets {
				return fmt.Errorf(
					"packing %d sets under a %d-set descriptor",
					e.Vector.Arity(), e.View.Sets)
			}
		case ExpandExpr:
			if err := e.View.Validate(); err != nil {
				return err
			}
			return visit(e.Input, depth+1)
		case HoleExpr:
			return nil
		case SelectExpr:
			return visit(e.Input, depth+1)
		case MapBoolExpr:
			if err := visitVec(e.Vector, depth+1); err != nil {
				return err
			}
			return visitBool(e.Body, depth+1)
		default:
			return fmt.Errorf("unsupported expression type %T", expression)
		}
		return nil
	}
	return start(walkers{expr: visit, intVec: visitIntVec})
}

func appendExpression(out []byte, expression Expr) []byte {
	appendU64 := func(value uint64) { out = binary.LittleEndian.AppendUint64(out, value) }
	appendView := func(view ViewSpec) {
		out = binary.LittleEndian.AppendUint32(out, view.Sets)
		out = append(out, byte(view.Layout))
		out = binary.LittleEndian.AppendUint64(out, view.Stride)
	}
	switch e := expression.(type) {
	case EmptyExpr:
		out = append(out, tagEmpty)
	case KeyExpr:
		out = append(out, tagKey)
		appendU64(e.Key)
	case RangeExpr:
		out = append(out, tagRange)
		appendU64(e.Lo)
		appendU64(e.Hi)
	case LiteralExpr:
		out = append(out, tagLiteral)
		out = binary.LittleEndian.AppendUint32(out, uint32(len(e.Ordinals)))
		for _, ordinal := range e.Ordinals {
			appendU64(ordinal)
		}
	case AndExpr:
		out = append(out, tagAnd)
		out = binary.LittleEndian.AppendUint16(out, uint16(len(e.Operands)))
		for _, child := range e.Operands {
			out = appendExpression(out, child)
		}
	case OrExpr:
		out = append(out, tagOr)
		out = binary.LittleEndian.AppendUint16(out, uint16(len(e.Operands)))
		for _, child := range e.Operands {
			out = appendExpression(out, child)
		}
	case AndNotExpr:
		out = append(out, tagAndNot)
		out = appendExpression(out, e.Include)
		out = appendExpression(out, e.Exclude)
	case AtExpr:
		out = append(out, tagAt)
		out = appendVector(out, e.Vector)
		out = binary.LittleEndian.AppendUint32(out, e.Index)
	case FoldExpr:
		out = append(out, tagFold)
		out = appendVector(out, e.Vector)
		out = append(out, byte(e.Op))
	case PackExpr:
		out = append(out, tagPack)
		appendView(e.View)
		out = appendVector(out, e.Vector)
	case ExpandExpr:
		out = append(out, tagExpand)
		appendView(e.View)
		out = appendExpression(out, e.Input)
	case HoleExpr:
		out = append(out, tagHole)
	case SelectExpr:
		out = append(out, tagSelect)
		appendU64(e.Index)
		out = appendExpression(out, e.Input)
	case MapBoolExpr:
		out = append(out, tagMapBool)
		out = appendVector(out, e.Vector)
		out = appendBool(out, e.Body)
	}
	return out
}

func appendBool(out []byte, expression BoolExpr) []byte {
	if e, ok := expression.(ContainsExpr); ok {
		out = append(out, tagContains)
		out = binary.LittleEndian.AppendUint64(out, e.Ordinal)
		out = appendExpression(out, e.Input)
	}
	return out
}

func appendInt(out []byte, expression IntExpr) []byte {
	switch e := expression.(type) {
	case IntLitExpr:
		out = append(out, tagIntLit)
		out = binary.LittleEndian.AppendUint64(out, e.Value)
	case CardinalityExpr:
		out = append(out, tagCardinality)
		out = appendExpression(out, e.Input)
	case RankExpr:
		out = append(out, tagRank)
		out = binary.LittleEndian.AppendUint64(out, e.Position)
		out = appendExpression(out, e.Input)
	case IntAtExpr:
		out = append(out, tagIntAt)
		out = appendIntVector(out, e.Vector)
		out = binary.LittleEndian.AppendUint32(out, e.Index)
	}
	return out
}

func appendIntVector(out []byte, vector VecIntExpr) []byte {
	switch v := vector.(type) {
	case IntListExpr:
		out = append(out, tagIntList)
		out = binary.LittleEndian.AppendUint16(out, uint16(len(v.Elements)))
		for _, element := range v.Elements {
			out = appendInt(out, element)
		}
	case MapIntExpr:
		out = append(out, tagMapInt)
		out = appendVector(out, v.Vector)
		out = appendInt(out, v.Body)
	}
	return out
}

func appendVector(out []byte, vector VecExpr) []byte {
	switch v := vector.(type) {
	case ListExpr:
		out = append(out, tagList)
		out = binary.LittleEndian.AppendUint16(out, uint16(len(v.Elements)))
		for _, element := range v.Elements {
			out = appendExpression(out, element)
		}
	case ViewExpr:
		out = append(out, tagView)
		out = binary.LittleEndian.AppendUint32(out, v.View.Sets)
		out = append(out, byte(v.View.Layout))
		out = binary.LittleEndian.AppendUint64(out, v.View.Stride)
		out = appendExpression(out, v.Input)
	case MapSetExpr:
		out = append(out, tagMapSet)
		out = appendVector(out, v.Vector)
		out = appendExpression(out, v.Body)
	}
	return out
}

// LooksLikeExpression distinguishes a YSNX command from a bare eight-byte key.
func LooksLikeExpression(payload []byte) bool {
	return len(payload) != 8 && len(payload) >= expressionHeader && string(payload[:4]) == string(expressionMagic[:])
}

// DecodeExpression decodes one complete YSNX v1 command.
func DecodeExpression(payload []byte) (Expr, error) {
	if len(payload) < expressionHeader {
		return nil, errors.New("expression ended in its header")
	}
	if string(payload[:4]) != string(expressionMagic[:]) {
		return nil, errors.New("not a yesnodb expression")
	}
	if payload[4] != expressionVersion || payload[5] != 0 {
		return nil, fmt.Errorf("unsupported expression version %d", payload[4])
	}
	cursor := expressionCursor{payload: payload[expressionHeader:]}
	expression, err := cursor.expression(0)
	if err != nil {
		return nil, err
	}
	if cursor.offset != len(cursor.payload) {
		return nil, errors.New("trailing bytes after expression")
	}
	return expression, nil
}

type expressionCursor struct {
	// inMap reports whether decoding is inside a map body. A bool rather than
	// a counter because nesting is refused, not tracked.
	inMap   bool
	payload []byte
	offset  int
	nodes   int
}

func (c *expressionCursor) take(size int) ([]byte, error) {
	if size < 0 || c.offset > len(c.payload)-size {
		return nil, errors.New("expression ended mid-node")
	}
	value := c.payload[c.offset : c.offset+size]
	c.offset += size
	return value, nil
}

func (c *expressionCursor) uint64() (uint64, error) {
	value, err := c.take(8)
	if err != nil {
		return 0, err
	}
	return binary.LittleEndian.Uint64(value), nil
}

func (c *expressionCursor) view() (ViewSpec, error) {
	rawSets, err := c.take(4)
	if err != nil {
		return ViewSpec{}, err
	}
	rawLayout, err := c.take(1)
	if err != nil {
		return ViewSpec{}, err
	}
	stride, err := c.uint64()
	if err != nil {
		return ViewSpec{}, err
	}
	view := ViewSpec{Sets: binary.LittleEndian.Uint32(rawSets), Layout: ViewLayout(rawLayout[0]), Stride: stride}
	if err := view.Validate(); err != nil {
		return ViewSpec{}, err
	}
	return view, nil
}

func (c *expressionCursor) expression(depth int) (Expr, error) {
	if depth > MaxExpressionDepth {
		return nil, fmt.Errorf("expression nested deeper than %d", MaxExpressionDepth)
	}
	c.nodes++
	if c.nodes > MaxExpressionNodes {
		return nil, fmt.Errorf("expression has more than %d nodes", MaxExpressionNodes)
	}
	rawTag, err := c.take(1)
	if err != nil {
		return nil, err
	}
	switch rawTag[0] {
	case tagEmpty:
		return Empty(), nil
	case tagKey:
		key, err := c.uint64()
		return Key(key), err
	case tagRange:
		lo, err := c.uint64()
		if err != nil {
			return nil, err
		}
		hi, err := c.uint64()
		return Range(lo, hi), err
	case tagLiteral:
		rawCount, err := c.take(4)
		if err != nil {
			return nil, err
		}
		count := int(binary.LittleEndian.Uint32(rawCount))
		if count > (len(c.payload)-c.offset)/8 {
			return nil, errors.New("expression ended mid-node")
		}
		ordinals := make([]uint64, count)
		for index := range ordinals {
			ordinal, err := c.uint64()
			if err != nil {
				return nil, err
			}
			if ordinal == math.MaxUint64 {
				return nil, errors.New("18446744073709551615 is outside the ordinal universe")
			}
			if index > 0 && ordinals[index-1] >= ordinal {
				return nil, errors.New("ordinal-set literal is not strictly ascending and unique")
			}
			ordinals[index] = ordinal
		}
		return LiteralExpr{Ordinals: ordinals}, nil
	case tagAnd, tagOr:
		rawCount, err := c.take(2)
		if err != nil {
			return nil, err
		}
		count := int(binary.LittleEndian.Uint16(rawCount))
		if count == 0 {
			return nil, errors.New("AND/OR has no operands")
		}
		if c.nodes+count > MaxExpressionNodes {
			return nil, fmt.Errorf("expression has more than %d nodes", MaxExpressionNodes)
		}
		operands := make([]Expr, 0, count)
		for range count {
			child, err := c.expression(depth + 1)
			if err != nil {
				return nil, err
			}
			operands = append(operands, child)
		}
		if rawTag[0] == tagAnd {
			return And(operands...), nil
		}
		return Or(operands...), nil
	case tagAndNot:
		include, err := c.expression(depth + 1)
		if err != nil {
			return nil, err
		}
		exclude, err := c.expression(depth + 1)
		if err != nil {
			return nil, err
		}
		return AndNot(include, exclude), nil
	case tagAt:
		vector, err := c.vector(depth + 1)
		if err != nil {
			return nil, err
		}
		rawIndex, err := c.take(4)
		if err != nil {
			return nil, err
		}
		index := binary.LittleEndian.Uint32(rawIndex)
		if index >= vector.Arity() {
			return nil, errors.New("index is at or above the vector's arity")
		}
		return At(vector, index), nil
	case tagFold:
		vector, err := c.vector(depth + 1)
		if err != nil {
			return nil, err
		}
		rawOp, err := c.take(1)
		if err != nil {
			return nil, err
		}
		op := FoldOp(rawOp[0])
		if op > FoldXor {
			return nil, fmt.Errorf("unknown fold operator %d", op)
		}
		return Fold(vector, op), nil
	case tagPack:
		view, err := c.view()
		if err != nil {
			return nil, err
		}
		vector, err := c.vector(depth + 1)
		if err != nil {
			return nil, err
		}
		if vector.Arity() != view.Sets {
			return nil, fmt.Errorf(
				"packing %d sets under a %d-set descriptor", vector.Arity(), view.Sets)
		}
		return Pack(vector, view), nil
	case tagExpand:
		view, err := c.view()
		if err != nil {
			return nil, err
		}
		input, err := c.expression(depth + 1)
		if err != nil {
			return nil, err
		}
		return Expand(input, view), nil
	case tagHole:
		if !c.inMap {
			return nil, errors.New("`_` outside a map body")
		}
		return Hole(), nil
	case tagSelect:
		index, err := c.uint64()
		if err != nil {
			return nil, err
		}
		input, err := c.expression(depth + 1)
		if err != nil {
			return nil, err
		}
		return Select(input, index), nil
	case tagMapBool:
		vector, err := c.vector(depth + 1)
		if err != nil {
			return nil, err
		}
		b, err := body(c, depth+1, c.boolExpression)
		if err != nil {
			return nil, err
		}
		return MapBool(vector, b), nil
	default:
		return nil, misplaced(sortSet, rawTag[0])
	}
}

// body decodes a map body, the only place a hole is legal.
//
// Refuses a nested body outright and restores the flag afterwards, so a map in
// a *vector* position -- sequential rather than nested -- still decodes.
func body[T any](c *expressionCursor, depth int, decode func(int) (T, error)) (T, error) {
	var zero T
	if c.inMap {
		return zero, errors.New("a map body may not contain a map")
	}
	c.inMap = true
	defer func() { c.inMap = false }()
	return decode(depth)
}

func (c *expressionCursor) boolExpression(depth int) (BoolExpr, error) {
	if depth > MaxExpressionDepth {
		return nil, fmt.Errorf("expression nested deeper than %d", MaxExpressionDepth)
	}
	c.nodes++
	if c.nodes > MaxExpressionNodes {
		return nil, fmt.Errorf("expression has more than %d nodes", MaxExpressionNodes)
	}
	rawTag, err := c.take(1)
	if err != nil {
		return nil, err
	}
	if rawTag[0] != tagContains {
		return nil, misplaced(sortBool, rawTag[0])
	}
	ordinal, err := c.uint64()
	if err != nil {
		return nil, err
	}
	input, err := c.expression(depth + 1)
	if err != nil {
		return nil, err
	}
	return Contains(input, ordinal), nil
}

func (c *expressionCursor) intExpression(depth int) (IntExpr, error) {
	if depth > MaxExpressionDepth {
		return nil, fmt.Errorf("expression nested deeper than %d", MaxExpressionDepth)
	}
	c.nodes++
	if c.nodes > MaxExpressionNodes {
		return nil, fmt.Errorf("expression has more than %d nodes", MaxExpressionNodes)
	}
	rawTag, err := c.take(1)
	if err != nil {
		return nil, err
	}
	switch rawTag[0] {
	case tagIntLit:
		value, err := c.uint64()
		if err != nil {
			return nil, err
		}
		return IntLit(value), nil
	case tagCardinality:
		input, err := c.expression(depth + 1)
		if err != nil {
			return nil, err
		}
		return Cardinality(input), nil
	case tagRank:
		position, err := c.uint64()
		if err != nil {
			return nil, err
		}
		input, err := c.expression(depth + 1)
		if err != nil {
			return nil, err
		}
		return Rank(input, position), nil
	case tagIntAt:
		vector, err := c.intVector(depth + 1)
		if err != nil {
			return nil, err
		}
		rawIndex, err := c.take(4)
		if err != nil {
			return nil, err
		}
		index := binary.LittleEndian.Uint32(rawIndex)
		if index >= vector.Arity() {
			return nil, errors.New("index is at or above the vector's arity")
		}
		return IntAt(vector, index), nil
	default:
		return nil, misplaced(sortInt, rawTag[0])
	}
}

func (c *expressionCursor) intVector(depth int) (VecIntExpr, error) {
	if depth > MaxExpressionDepth {
		return nil, fmt.Errorf("expression nested deeper than %d", MaxExpressionDepth)
	}
	c.nodes++
	if c.nodes > MaxExpressionNodes {
		return nil, fmt.Errorf("expression has more than %d nodes", MaxExpressionNodes)
	}
	rawTag, err := c.take(1)
	if err != nil {
		return nil, err
	}
	switch rawTag[0] {
	case tagIntList:
		rawCount, err := c.take(2)
		if err != nil {
			return nil, err
		}
		count := int(binary.LittleEndian.Uint16(rawCount))
		if count == 0 {
			return nil, errors.New("vector has no elements")
		}
		if c.nodes+count > MaxExpressionNodes {
			return nil, fmt.Errorf("expression has more than %d nodes", MaxExpressionNodes)
		}
		elements := make([]IntExpr, 0, count)
		for range count {
			element, err := c.intExpression(depth + 1)
			if err != nil {
				return nil, err
			}
			elements = append(elements, element)
		}
		return IntList(elements...), nil
	case tagMapInt:
		vector, err := c.vector(depth + 1)
		if err != nil {
			return nil, err
		}
		b, err := body(c, depth+1, c.intExpression)
		if err != nil {
			return nil, err
		}
		return MapInt(vector, b), nil
	default:
		return nil, misplaced(sortVecInt, rawTag[0])
	}
}

// vector decodes a vector-sorted node.
//
// The mirror of expression, and the reason both tag ranges share one space: a
// set-sorted tag arriving here is a sort error naming both sides, not a
// reinterpretation of whatever that byte means in this position.
func (c *expressionCursor) vector(depth int) (VecExpr, error) {
	if depth > MaxExpressionDepth {
		return nil, fmt.Errorf("expression nested deeper than %d", MaxExpressionDepth)
	}
	c.nodes++
	if c.nodes > MaxExpressionNodes {
		return nil, fmt.Errorf("expression has more than %d nodes", MaxExpressionNodes)
	}
	rawTag, err := c.take(1)
	if err != nil {
		return nil, err
	}
	switch rawTag[0] {
	case tagList:
		rawCount, err := c.take(2)
		if err != nil {
			return nil, err
		}
		count := int(binary.LittleEndian.Uint16(rawCount))
		if count == 0 {
			return nil, errors.New("vector has no elements")
		}
		if c.nodes+count > MaxExpressionNodes {
			return nil, fmt.Errorf("expression has more than %d nodes", MaxExpressionNodes)
		}
		elements := make([]Expr, 0, count)
		for range count {
			element, err := c.expression(depth + 1)
			if err != nil {
				return nil, err
			}
			elements = append(elements, element)
		}
		return List(elements...), nil
	case tagView:
		view, err := c.view()
		if err != nil {
			return nil, err
		}
		input, err := c.expression(depth + 1)
		if err != nil {
			return nil, err
		}
		return View(input, view), nil
	case tagMapSet:
		vector, err := c.vector(depth + 1)
		if err != nil {
			return nil, err
		}
		b, err := body(c, depth+1, c.expression)
		if err != nil {
			return nil, err
		}
		return MapSet(vector, b), nil
	default:
		return nil, misplaced(sortVecSet, rawTag[0])
	}
}

// QueryRequest binds an expression to the current or an exact database version.
type QueryRequest struct {
	Expression Expr
	Version    *uint64
}

// CurrentQuery constructs a request evaluated at the server's current version.
func CurrentQuery(expression Expr) QueryRequest {
	return QueryRequest{Expression: expression}
}

// QueryAt constructs a strict exact-version request.
func QueryAt(expression Expr, version uint64) QueryRequest {
	return QueryRequest{Expression: expression, Version: &version}
}

// Encode returns the complete YSNQ v1 descriptor command.
func (q QueryRequest) Encode() ([]byte, error) {
	expression, err := EncodeExpression(q.Expression)
	if err != nil {
		return nil, err
	}
	out := make([]byte, 0, queryHeader+len(expression))
	out = append(out, queryMagic[:]...)
	out = append(out, queryVersion)
	if q.Version != nil {
		out = append(out, queryFlagPinned)
		out = binary.LittleEndian.AppendUint64(out, *q.Version)
	} else {
		out = append(out, 0)
		out = binary.LittleEndian.AppendUint64(out, 0)
	}
	return append(out, expression...), nil
}

// LooksLikeQueryRequest reports whether a descriptor starts with YSNQ v1 framing.
func LooksLikeQueryRequest(payload []byte) bool {
	return len(payload) >= queryHeader && string(payload[:4]) == string(queryMagic[:])
}

// DecodeQueryRequest decodes one complete YSNQ v1 descriptor command.
func DecodeQueryRequest(payload []byte) (QueryRequest, error) {
	if len(payload) < queryHeader {
		return QueryRequest{}, errors.New("query request ended in its header")
	}
	if string(payload[:4]) != string(queryMagic[:]) {
		return QueryRequest{}, errors.New("not a yesnodb query request")
	}
	if payload[4] != queryVersion || payload[5]&^queryFlagPinned != 0 {
		return QueryRequest{}, fmt.Errorf("unsupported query request version %d", payload[4])
	}
	rawVersion := binary.LittleEndian.Uint64(payload[6:queryHeader])
	pinned := payload[5]&queryFlagPinned != 0
	if !pinned && rawVersion != 0 {
		return QueryRequest{}, errors.New("unpinned request carries a non-zero version")
	}
	expression, err := DecodeExpression(payload[queryHeader:])
	if err != nil {
		return QueryRequest{}, err
	}
	if pinned {
		return QueryAt(expression, rawVersion), nil
	}
	return CurrentQuery(expression), nil
}
