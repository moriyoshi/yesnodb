//go:build integration

package yesnodb

import (
	"bytes"
	"context"
	"crypto/sha256"
	"crypto/tls"
	"crypto/x509"
	"errors"
	"fmt"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"reflect"
	"strconv"
	"strings"
	"syscall"
	"testing"
	"time"

	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

type serverMode int

const (
	serverOpen serverMode = iota
	serverBearer
	serverMutualTLS
)

type runningServer struct {
	address string
	tlsDir  string
}

func freePort(t *testing.T) int {
	t.Helper()
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	port := listener.Addr().(*net.TCPAddr).Port
	if err := listener.Close(); err != nil {
		t.Fatal(err)
	}
	return port
}

func tokenDigest(token string) string {
	digest := sha256.Sum256([]byte(token))
	return fmt.Sprintf("%x", digest[:])
}

func startServer(t *testing.T, mode serverMode) runningServer {
	t.Helper()
	binary := os.Getenv("YESNODB_TEST_SERVER")
	if binary == "" {
		t.Fatal("YESNODB_TEST_SERVER must name the shipped yesnod binary")
	}
	if _, err := os.Stat(binary); err != nil {
		t.Fatalf("YESNODB_TEST_SERVER: %v", err)
	}
	directory := t.TempDir()
	port := freePort(t)
	tlsDir := ""
	if mode == serverMutualTLS {
		tlsDir = filepath.Join(directory, "tls")
		generator := filepath.Join("..", "yesno-server", "dist", "gen-dev-certs.sh")
		command := exec.Command(generator, tlsDir)
		if output, err := command.CombinedOutput(); err != nil {
			t.Fatalf("generate TLS certificates: %v\n%s", err, output)
		}
	}
	config := strings.Builder{}
	fmt.Fprintf(&config, "[server]\ndata_dir = %q\nrole = \"leader\"\n", filepath.Join(directory, "data"))
	fmt.Fprintf(&config, "\n[server.flight]\nlisten = \"127.0.0.1:%d\"\n", port)
	config.WriteString("\n[server.metrics]\nlisten = \"\"\n")
	config.WriteString("\n[server.control]\nlisten = \"\"\njournal_dir = \"\"\n")
	if mode == serverBearer {
		config.WriteString("\n[auth]\nanonymous = \"none\"\n")
		for _, principal := range []struct{ name, role, token string }{
			{"reader", "reader", "read-token"},
			{"writer", "writer", "write-token"},
			{"admin", "admin", "admin-token"},
		} {
			fmt.Fprintf(&config, "\n[[auth.principal]]\nname = %q\nrole = %q\ntoken_sha256 = %q\n", principal.name, principal.role, tokenDigest(principal.token))
		}
	}
	if mode == serverMutualTLS {
		fingerprint, err := os.ReadFile(filepath.Join(tlsDir, "client.sha256"))
		if err != nil {
			t.Fatal(err)
		}
		config.WriteString("\n[auth]\nanonymous = \"none\"\n")
		fmt.Fprintf(&config, "\n[[auth.principal]]\nname = \"client\"\nrole = \"writer\"\ncert_sha256 = %q\n", strings.TrimSpace(string(fingerprint)))
		fmt.Fprintf(&config, "\n[server.flight.tls]\ncert = %q\nkey = %q\nclient_ca = %q\nrequire_client_auth = true\n",
			filepath.Join(tlsDir, "server.pem"), filepath.Join(tlsDir, "server.key"), filepath.Join(tlsDir, "ca.pem"))
	}
	configPath := filepath.Join(directory, "yesnod.toml")
	if err := os.WriteFile(configPath, []byte(config.String()), 0o600); err != nil {
		t.Fatal(err)
	}
	var output bytes.Buffer
	command := exec.Command(binary, "--config", configPath, "--log", "warn")
	command.Dir = ".."
	command.Stdout = &output
	command.Stderr = &output
	if err := command.Start(); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		if command.ProcessState == nil || !command.ProcessState.Exited() {
			_ = command.Process.Signal(syscall.SIGTERM)
		}
		done := make(chan error, 1)
		go func() { done <- command.Wait() }()
		select {
		case <-done:
		case <-time.After(10 * time.Second):
			_ = command.Process.Kill()
			<-done
		}
	})
	address := net.JoinHostPort("127.0.0.1", strconv.Itoa(port))
	deadline := time.Now().Add(10 * time.Second)
	for {
		connection, err := net.DialTimeout("tcp", address, 200*time.Millisecond)
		if err == nil {
			_ = connection.Close()
			break
		}
		if command.ProcessState != nil && command.ProcessState.Exited() {
			t.Fatalf("yesnod exited during startup:\n%s", output.String())
		}
		if time.Now().After(deadline) {
			t.Fatalf("yesnod did not listen within 10 seconds:\n%s", output.String())
		}
		time.Sleep(20 * time.Millisecond)
	}
	return runningServer{address: address, tlsDir: tlsDir}
}

func TestClientCoversShippedFlightSurface(t *testing.T) {
	server := startServer(t, serverOpen)
	client, err := Dial(server.address, WithInsecureTransport())
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = client.Close() })
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()

	pairs := make([]Pair, 9_000)
	for ordinal := range pairs {
		pairs[ordinal] = Pair{Key: 7, Ordinal: uint64(ordinal)}
	}
	if count, err := client.Insert(ctx, pairs); err != nil || count != 9_000 {
		t.Fatalf("Insert = %d, %v", count, err)
	}
	if count, err := client.Insert(ctx, nil); err != nil || count != 0 {
		t.Fatalf("empty Insert = %d, %v", count, err)
	}
	if keys, err := client.Keys(ctx); err != nil || len(keys) != 1 || keys[0] != 7 {
		t.Fatalf("Keys = %v, %v", keys, err)
	}
	planned, err := client.PrepareKey(ctx, 7)
	if err != nil {
		t.Fatal(err)
	}
	if planned.TotalRecords() != 9_000 || planned.Version() == 0 || planned.Ticket().Key != 7 {
		t.Fatalf("planned = count %d, version %d, ticket %#v", planned.TotalRecords(), planned.Version(), planned.Ticket())
	}
	if count, err := client.InsertBatch(ctx, []Pair{{Key: 7, Ordinal: 20_000}}); err != nil || count != 1 {
		t.Fatalf("InsertBatch = %d, %v", count, err)
	}
	pinned, err := client.Fetch(ctx, planned)
	if err != nil {
		t.Fatal(err)
	}
	ordinals, err := pinned.CollectOrdinals()
	pinned.Release()
	if err != nil || len(ordinals) != 9_000 || ordinals[0] != 0 || ordinals[len(ordinals)-1] != 8_999 {
		t.Fatalf("pinned result len=%d first=%d last=%d err=%v", len(ordinals), ordinals[0], ordinals[len(ordinals)-1], err)
	}
	explicit, err := client.PrepareQueryAt(ctx, Key(7), planned.Version())
	if err != nil || explicit.TotalRecords() != 9_000 || explicit.Version() != planned.Version() {
		t.Fatalf("explicit plan = %#v, %v", explicit, err)
	}

	even := make([]Pair, 0, 4_500)
	for ordinal := uint64(0); ordinal < 9_000; ordinal += 2 {
		even = append(even, Pair{Key: 8, Ordinal: ordinal})
	}
	if _, err := client.Insert(ctx, even); err != nil {
		t.Fatal(err)
	}
	expression := And(Key(7), Or(Key(8), Range(20_000, 20_001)))
	if count, err := client.QueryCardinality(ctx, expression); err != nil || count != 4_501 {
		t.Fatalf("QueryCardinality = %d, %v", count, err)
	}
	query, err := client.Query(ctx, expression)
	if err != nil {
		t.Fatal(err)
	}
	result, err := query.CollectOrdinals()
	query.Release()
	if err != nil || len(result) != 4_501 || result[len(result)-1] != 20_000 {
		t.Fatalf("query result len=%d last=%d err=%v", len(result), result[len(result)-1], err)
	}
	literalLeaf, err := Literal(9, 0, 9, 65_536, 5)
	if err != nil {
		t.Fatal(err)
	}
	literalQuery, err := client.Query(ctx, And(Key(7), literalLeaf))
	if err != nil {
		t.Fatal(err)
	}
	literalResult, err := literalQuery.CollectOrdinals()
	literalQuery.Release()
	if err != nil || !reflect.DeepEqual(literalResult, []uint64{0, 5, 9}) {
		t.Fatalf("literal result = %v, %v", literalResult, err)
	}

	packed := []Pair{{9, 7}, {9, 25}, {9, 268}}
	if _, err := client.InsertBatch(ctx, packed); err != nil {
		t.Fatal(err)
	}
	view, err := client.Query(ctx, At(View(Key(9), InterleavedView(3)), 1))
	if err != nil {
		t.Fatal(err)
	}
	viewResult, err := view.CollectOrdinals()
	view.Release()
	if err != nil || fmt.Sprint(viewResult) != "[2 8 89]" {
		t.Fatalf("view result = %v, %v", viewResult, err)
	}

	if changed, err := client.InsertOne(ctx, 10, 5); err != nil || !changed {
		t.Fatalf("first InsertOne = %v, %v", changed, err)
	}
	if changed, err := client.InsertOne(ctx, 10, 5); err != nil || changed {
		t.Fatalf("duplicate InsertOne = %v, %v", changed, err)
	}
	if present, err := client.Contains(ctx, 10, 5); err != nil || !present {
		t.Fatalf("Contains = %v, %v", present, err)
	}
	if changed, err := client.RemoveOne(ctx, 10, 5); err != nil || !changed {
		t.Fatalf("RemoveOne = %v, %v", changed, err)
	}
	if changed, err := client.RemoveOne(ctx, 10, 5); err != nil || changed {
		t.Fatalf("missing RemoveOne = %v, %v", changed, err)
	}
	if _, err := client.Clear(ctx, 9); err != nil {
		t.Fatal(err)
	}
	if count, err := client.Cardinality(ctx, 9); err != nil || count != 0 {
		t.Fatalf("cleared cardinality = %d, %v", count, err)
	}
	stats, err := client.Stats(ctx)
	if err != nil || stats.Shards == 0 || stats.WALBytes == 0 {
		t.Fatalf("Stats = %#v, %v", stats, err)
	}
}

func TestBearerRolesAndLeadershipFence(t *testing.T) {
	server := startServer(t, serverBearer)
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()

	writer, err := Dial(server.address, WithInsecureTransport(), WithBearerToken("write-token"))
	if err != nil {
		t.Fatal(err)
	}
	defer writer.Close()
	if _, err := writer.InsertBatch(ctx, []Pair{{1, 10}}); err != nil {
		t.Fatal(err)
	}
	if _, err := writer.Clear(ctx, 1); status.Code(err) != codes.PermissionDenied {
		t.Fatalf("writer clear code = %s, %v", status.Code(err), err)
	}

	reader, err := Dial(server.address, WithInsecureTransport(), WithBearerToken("read-token"))
	if err != nil {
		t.Fatal(err)
	}
	defer reader.Close()
	if count, err := reader.Cardinality(ctx, 1); err != nil || count != 1 {
		t.Fatalf("reader Cardinality = %d, %v", count, err)
	}
	if _, err := reader.InsertBatch(ctx, []Pair{{1, 11}}); status.Code(err) != codes.PermissionDenied {
		t.Fatalf("reader insert code = %s, %v", status.Code(err), err)
	}

	admin, err := Dial(server.address, WithInsecureTransport(), WithBearerToken("admin-token"))
	if err != nil {
		t.Fatal(err)
	}
	defer admin.Close()
	if _, err := admin.Clear(ctx, 1); err != nil {
		t.Fatal(err)
	}

	unknown, err := Dial(server.address, WithInsecureTransport(), WithBearerToken("wrong-token"))
	if err != nil {
		t.Fatal(err)
	}
	defer unknown.Close()
	if _, err := unknown.Cardinality(ctx, 1); status.Code(err) != codes.Unauthenticated {
		t.Fatalf("unknown token code = %s, %v", status.Code(err), err)
	}

	fenced, err := Dial(server.address, WithInsecureTransport(), WithBearerToken("read-token"), WithMinimumTerm(^uint32(0)))
	if err != nil {
		t.Fatal(err)
	}
	defer fenced.Close()
	if _, err := fenced.Cardinality(ctx, 1); status.Code(err) != codes.FailedPrecondition {
		t.Fatalf("fenced code = %s, %v", status.Code(err), err)
	}
}

func TestMutualTLSWriter(t *testing.T) {
	server := startServer(t, serverMutualTLS)
	caPEM, err := os.ReadFile(filepath.Join(server.tlsDir, "ca.pem"))
	if err != nil {
		t.Fatal(err)
	}
	roots := x509.NewCertPool()
	if !roots.AppendCertsFromPEM(caPEM) {
		t.Fatal("could not parse test CA")
	}
	certificate, err := tls.LoadX509KeyPair(filepath.Join(server.tlsDir, "client.pem"), filepath.Join(server.tlsDir, "client.key"))
	if err != nil {
		t.Fatal(err)
	}
	client, err := Dial(server.address, WithTLSConfig(&tls.Config{RootCAs: roots, Certificates: []tls.Certificate{certificate}, ServerName: "localhost", MinVersion: tls.VersionTLS12}))
	if err != nil {
		t.Fatal(err)
	}
	defer client.Close()
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	if _, err := client.InsertBatch(ctx, []Pair{{5, 99}}); err != nil {
		t.Fatal(err)
	}
	stream, err := client.Get(ctx, 5)
	if err != nil {
		t.Fatal(err)
	}
	result, err := stream.CollectOrdinals()
	stream.Release()
	if err != nil || fmt.Sprint(result) != "[99]" {
		t.Fatalf("mTLS result = %v, %v", result, err)
	}

	noIdentity, err := Dial(server.address, WithTLSConfig(&tls.Config{RootCAs: roots, ServerName: "localhost", MinVersion: tls.VersionTLS12}))
	if err != nil {
		t.Fatal(err)
	}
	defer noIdentity.Close()
	if _, err := noIdentity.Cardinality(ctx, 5); err == nil || (!errors.Is(err, context.DeadlineExceeded) && status.Code(err) == codes.OK) {
		t.Fatalf("TLS connection without client identity was accepted: %v", err)
	}
}

// TestWriteTransactionsAgainstRealServer covers the surface the unit tests
// cannot: Apply, BeginWrite, multi-call Stage, visibility before commit, the
// commit retry, and AbortWrite, all against the shipped yesnod.
//
// Added after a follow-up audit observed that Go exercised only the feature
// bits and the mutation constructors, so the whole transaction path was
// unverified in this client.
func TestWriteTransactionsAgainstRealServer(t *testing.T) {
	server := startServer(t, serverOpen)
	client, err := Dial(server.address, WithInsecureTransport())
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = client.Close() })
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()

	if ok, err := client.Supports(ctx, FeatureMixedPut|FeatureWriteTransactions); err != nil || !ok {
		t.Fatalf("Supports = %v, %v; the shipped server implements both", ok, err)
	}

	// One-shot mixed apply, ordered: the delete must not be reordered ahead of
	// the insert that follows it.
	if _, err := client.Insert(ctx, []Pair{{Key: 1, Ordinal: 10}, {Key: 1, Ordinal: 11}}); err != nil {
		t.Fatal(err)
	}
	ack, err := client.Apply(ctx, []Mutation{
		Remove(1, 10),
		Insert(1, 12),
		InsertRange(3, 100, 104),
		DeleteKey(1),
		Insert(1, 13),
	})
	if err != nil {
		t.Fatal(err)
	}
	if ack.Rows != 5 || ack.Version == 0 {
		t.Fatalf("Apply = %+v", ack)
	}
	if got := ordinalsOf(ctx, t, client, 1); !reflect.DeepEqual(got, []uint64{13}) {
		t.Fatalf("key 1 = %v; the delete must apply before the insert that follows it", got)
	}
	if got := ordinalsOf(ctx, t, client, 3); !reflect.DeepEqual(got, []uint64{100, 101, 102, 103, 104}) {
		t.Fatalf("key 3 = %v", got)
	}

	// A transaction across two Stage calls.
	txn, err := client.BeginWrite(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if rows, err := client.Stage(ctx, txn, []Mutation{Insert(8, 80), Insert(8, 81)}); err != nil || rows != 2 {
		t.Fatalf("Stage = %d, %v", rows, err)
	}
	if rows, err := client.Stage(ctx, txn, []Mutation{InsertRange(8, 90, 92)}); err != nil || rows != 1 {
		t.Fatalf("Stage = %d, %v", rows, err)
	}
	if got := ordinalsOf(ctx, t, client, 8); len(got) != 0 {
		t.Fatalf("key 8 = %v before commit; staged work must be invisible", got)
	}

	version, err := client.CommitWrite(ctx, txn)
	if err != nil || version == 0 {
		t.Fatalf("CommitWrite = %d, %v", version, err)
	}
	if got := ordinalsOf(ctx, t, client, 8); !reflect.DeepEqual(got, []uint64{80, 81, 90, 91, 92}) {
		t.Fatalf("key 8 = %v; everything staged appears at one version", got)
	}
	// Retry while the outcome is still resident returns the same version.
	if again, err := client.CommitWrite(ctx, txn); err != nil || again != version {
		t.Fatalf("commit retry = %d, %v; want %d", again, err, version)
	}

	// Abort discards, and aborting an unknown transaction still succeeds.
	aborted, err := client.BeginWrite(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := client.Stage(ctx, aborted, []Mutation{Insert(9, 90)}); err != nil {
		t.Fatal(err)
	}
	if err := client.AbortWrite(ctx, aborted); err != nil {
		t.Fatal(err)
	}
	if got := ordinalsOf(ctx, t, client, 9); len(got) != 0 {
		t.Fatalf("key 9 = %v after abort", got)
	}
	if err := client.AbortWrite(ctx, aborted); err != nil {
		t.Fatalf("aborting an unknown transaction must succeed: %v", err)
	}

	// A malformed mutation costs nothing, because this client validates the
	// whole slice before opening a stream: no rows are sent, so the
	// transaction is untouched and still usable.
	//
	// That is the difference between a client-side refusal and a server-side
	// one. A failure the client cannot see coming -- the row bound, a dropped
	// connection -- leaves earlier batches of the same call staged, and the
	// server then poisons the transaction so they cannot be published. Stage's
	// documentation says to treat any error as fatal for that reason; the
	// poison itself is asserted in yesno-flight's own tests, where a stream
	// can be driven past this client's validation.
	clean, err := client.BeginWrite(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := client.Stage(ctx, clean, []Mutation{Insert(4, 40), {Key: 5, Lo: 9, Hi: 4, Op: OpRemoveRange}}); err == nil {
		t.Fatal("an inverted range must be refused")
	}
	if _, err := client.Stage(ctx, clean, []Mutation{Insert(4, 41)}); err != nil {
		t.Fatalf("a locally refused Stage must not disturb the transaction: %v", err)
	}
	if _, err := client.CommitWrite(ctx, clean); err != nil {
		t.Fatal(err)
	}
	if got := ordinalsOf(ctx, t, client, 4); !reflect.DeepEqual(got, []uint64{41}) {
		t.Fatalf("key 4 = %v; the refused call sent nothing, including its valid row", got)
	}
}

func ordinalsOf(ctx context.Context, t *testing.T, client *Client, key uint64) []uint64 {
	t.Helper()
	stream, err := client.Get(ctx, key)
	if err != nil {
		t.Fatal(err)
	}
	defer stream.Release()
	got, err := stream.CollectOrdinals()
	if err != nil {
		t.Fatal(err)
	}
	return got
}
