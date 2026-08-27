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
	tagViewSelect
	tagViewFold
	tagViewExpand
	tagLiteral
)

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
type ViewReduce uint8

const (
	ViewAny ViewReduce = iota
	ViewAll
	ViewParity
)

// ViewSelectExpr extracts one logical constituent from a packed key.
type ViewSelectExpr struct {
	Key  uint64
	View ViewSpec
	Set  uint32
}

func (ViewSelectExpr) yesnoExpr() {}

// ViewFoldExpr reduces every constituent of a packed key.
type ViewFoldExpr struct {
	Key    uint64
	View   ViewSpec
	Reduce ViewReduce
}

func (ViewFoldExpr) yesnoExpr() {}

// ViewExpandExpr maps every logical input ordinal into every constituent.
type ViewExpandExpr struct {
	Input Expr
	View  ViewSpec
}

func (ViewExpandExpr) yesnoExpr() {}

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

// ViewSelect constructs a packed-view selection.
func ViewSelect(key uint64, view ViewSpec, set uint32) Expr {
	return ViewSelectExpr{Key: key, View: view, Set: set}
}

// ViewFold constructs a packed-view reduction.
func ViewFold(key uint64, view ViewSpec, reduce ViewReduce) Expr {
	return ViewFoldExpr{Key: key, View: view, Reduce: reduce}
}

// ViewExpand constructs a packed-view expansion.
func ViewExpand(input Expr, view ViewSpec) Expr {
	return ViewExpandExpr{Input: input, View: view}
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

func validateExpression(root Expr) error {
	nodes := 0
	var visit func(Expr, int) error
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
		case ViewSelectExpr:
			if err := e.View.Validate(); err != nil {
				return err
			}
			if e.Set >= e.View.Sets {
				return errors.New("view constituent is outside the descriptor")
			}
		case ViewFoldExpr:
			if err := e.View.Validate(); err != nil {
				return err
			}
			if e.Reduce > ViewParity {
				return fmt.Errorf("unknown view reduction %d", e.Reduce)
			}
		case ViewExpandExpr:
			if err := e.View.Validate(); err != nil {
				return err
			}
			return visit(e.Input, depth+1)
		default:
			return fmt.Errorf("unsupported expression type %T", expression)
		}
		return nil
	}
	return visit(root, 0)
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
	case ViewSelectExpr:
		out = append(out, tagViewSelect)
		appendU64(e.Key)
		appendView(e.View)
		out = binary.LittleEndian.AppendUint32(out, e.Set)
	case ViewFoldExpr:
		out = append(out, tagViewFold)
		appendU64(e.Key)
		appendView(e.View)
		out = append(out, byte(e.Reduce))
	case ViewExpandExpr:
		out = append(out, tagViewExpand)
		appendView(e.View)
		out = appendExpression(out, e.Input)
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
	case tagViewSelect:
		key, err := c.uint64()
		if err != nil {
			return nil, err
		}
		view, err := c.view()
		if err != nil {
			return nil, err
		}
		rawSet, err := c.take(4)
		if err != nil {
			return nil, err
		}
		set := binary.LittleEndian.Uint32(rawSet)
		if set >= view.Sets {
			return nil, errors.New("view constituent is outside the descriptor")
		}
		return ViewSelect(key, view, set), nil
	case tagViewFold:
		key, err := c.uint64()
		if err != nil {
			return nil, err
		}
		view, err := c.view()
		if err != nil {
			return nil, err
		}
		rawReduce, err := c.take(1)
		if err != nil {
			return nil, err
		}
		reduce := ViewReduce(rawReduce[0])
		if reduce > ViewParity {
			return nil, fmt.Errorf("unknown view reduction %d", reduce)
		}
		return ViewFold(key, view, reduce), nil
	case tagViewExpand:
		view, err := c.view()
		if err != nil {
			return nil, err
		}
		input, err := c.expression(depth + 1)
		if err != nil {
			return nil, err
		}
		return ViewExpand(input, view), nil
	default:
		return nil, fmt.Errorf("unknown expression tag %d", rawTag[0])
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
