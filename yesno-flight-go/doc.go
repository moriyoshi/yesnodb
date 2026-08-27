// Package yesnodb provides a typed Go client for the yesnodb Arrow Flight
// service.
//
// The package owns yesnodb's command, expression, ticket, and acknowledgement
// formats while exposing Apache Arrow record batches without copying them into
// Go objects. Every RPC accepts a context. Query planning and fetching are
// separate so a caller can retain a versioned ticket for a repeatable read.
package yesnodb
