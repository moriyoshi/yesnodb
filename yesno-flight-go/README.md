# yesnodb Arrow Flight client for Go

`yesno-flight-go` is the typed Go client for yesnodb's Arrow Flight service. It
uses Apache Arrow Go directly and covers versioned planning, streaming reads,
exact cardinality, Boolean, ordinal-set literal, and packed-view expressions, bulk and point
mutations, server statistics, bearer authentication, TLS, mutual TLS, and
leadership fencing.

The module requires Go 1.25 or newer.

```go
package main

import (
	"context"
	"fmt"
	"log"

	yesnodb "github.com/moriyoshi/yesnodb/yesno-flight-go"
)

func main() {
	client, err := yesnodb.Dial("127.0.0.1:50051", yesnodb.WithInsecureTransport())
	if err != nil {
		log.Fatal(err)
	}
	defer client.Close()

	ctx := context.Background()
	_, err = client.InsertBatch(ctx, []yesnodb.Pair{{Key: 42, Ordinal: 1}, {Key: 42, Ordinal: 5}})
	if err != nil {
		log.Fatal(err)
	}

	literal, err := yesnodb.Literal(5, 1, 5)
	if err != nil {
		log.Fatal(err)
	}
	stream, err := client.Query(ctx, yesnodb.And(yesnodb.Key(42), literal))
	if err != nil {
		log.Fatal(err)
	}
	defer stream.Release()
	ordinals, err := stream.CollectOrdinals()
	if err != nil {
		log.Fatal(err)
	}
	fmt.Println(ordinals)
}
```

## Writing whole sets

The pair wire costs one row per ordinal. `InsertContainers` costs one row per
65,536-ordinal chunk, carrying the Roaring-spec payload the page store itself
holds, which the server measured at 64.8x less wire on a half-dense page:

```go
ordinals := make([]uint64, 0, 333_334)
for ordinal := uint64(0); ordinal < 1_000_000; ordinal += 3 {
	ordinals = append(ordinals, ordinal)
}
if ok, err := client.Supports(ctx, yesnodb.FeatureContainerPut); err != nil {
	log.Fatal(err)
} else if ok {
	_, err = client.InsertContainers(ctx, []yesnodb.Set{{Key: 42, Ordinals: ordinals}})
} else {
	pairs := make([]yesnodb.Pair, 0, len(ordinals))
	for _, ordinal := range ordinals {
		pairs = append(pairs, yesnodb.Pair{Key: 42, Ordinal: ordinal})
	}
	_, err = client.InsertBatch(ctx, pairs)
}
if err != nil {
	log.Fatal(err)
}
```

One commit for the whole call, like `Apply`: every chunk becomes visible at the
single version the `Ack` reports, however many batches the sets were split
across. Container rows union into what is already under the key; there is no
removal form, because the write wire reuses the read side's schema unchanged
and that schema has no operation column. Remove with `RemoveRange` instead.

A caller that already holds containers -- restoring a dump, forwarding a
portable Roaring file -- builds batches with `ContainerBatchBuilder` and sends
them through `ApplyContainers`. The builder and the batches own Arrow memory:
release each batch, and release the builder when done with it.

## Consistency and streaming

`PrepareKey`, `PrepareQuery`, and `PrepareQueryAt` return a `QueryInfo` whose
exact count and ticket name one database snapshot. `Fetch` consumes that ticket
without replanning. Tickets are not leases: if checkpoint reclamation removes
the named version before `Fetch`, the server returns a gRPC failed-precondition
error and the caller must decide whether a fresh snapshot is acceptable.

`QueryStream.Record` is owned by Arrow's reader and remains valid until the next
`Next` call. Retain a record before keeping it longer, and always call
`QueryStream.Release`. The stream validates its schema, nullability, strict set
ordering, and promised cardinality across batch boundaries.

## Transport and authentication

Transport choice is explicit. Use `WithInsecureTransport` only for plaintext
endpoints, or pass a `tls.Config` through `WithTLSConfig`. `WithBearerToken` and
`WithTokenProvider` send authorization on every unary and streaming RPC.

`WithMinimumTerm` initializes the leadership fence. The client learns newer
`yesno-term` response metadata monotonically and sends the current floor as
`yesno-expect-term` on later calls.

## Gate

From the repository root:

```console
./yesno-flight-go/gate.sh
```

The gate checks formatting, module tidiness, `go vet`, the race detector, and a
live round trip through the exact `yesnod` built from this checkout. Temporary
server data stays under `.agents-workspace/tmp`.
