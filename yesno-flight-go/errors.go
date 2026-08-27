package yesnodb

import "fmt"

// ProtocolError reports a malformed yesnodb request or response.
//
// Transport errors remain ordinary gRPC errors so callers can inspect them
// with status.Code. ProtocolError is reserved for bytes or Arrow values that
// crossed the transport successfully but violate the yesnodb contract.
type ProtocolError struct {
	Operation string
	Problem   string
}

func (e *ProtocolError) Error() string {
	if e.Operation == "" {
		return "yesnodb protocol error: " + e.Problem
	}
	return fmt.Sprintf("yesnodb %s: %s", e.Operation, e.Problem)
}

func protocolError(operation, problem string) error {
	return &ProtocolError{Operation: operation, Problem: problem}
}

func protocolErrorf(operation, format string, args ...any) error {
	return protocolError(operation, fmt.Sprintf(format, args...))
}
