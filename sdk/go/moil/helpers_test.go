package moil_test

import (
	"bytes"
	"context"
	"encoding/json"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"slices"
	"sync"
	"testing"
	"time"

	"git.convex.works/ConvexWorks/moil/sdk/go/moil"
)

// newServer starts a Server with durations short enough for tests, and
// closes it when the test ends.
func newServer(t *testing.T, configure ...func(*moil.Config)) *moil.Server {
	t.Helper()
	cfg := moil.Config{
		Name:            "klisi",
		VerificationURL: "https://klisi.example.com/machines/pair",
		Store:           moil.NewMemoryStore(),
		Logger:          slog.New(slog.NewTextHandler(testLog{t}, &slog.HandlerOptions{Level: slog.LevelDebug})),
		PairingInterval: time.Nanosecond, // advertised as 1s; tests poll back to back
		BidWindow:       5 * time.Second,
		LeaseTTL:        time.Second,
		PingInterval:    5 * time.Second,
	}
	for _, f := range configure {
		f(&cfg)
	}
	srv, err := moil.NewServer(cfg)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { srv.Close() })
	return srv
}

// testLog sends the Server's logs to the test's log, shown on failure or
// with -v.
type testLog struct{ t *testing.T }

func (l testLog) Write(p []byte) (int, error) {
	l.t.Helper()
	l.t.Log(string(bytes.TrimSuffix(p, []byte("\n"))))
	return len(p), nil
}

// vectorBundle loads a bundle from spec/vectors/bundles.
func vectorBundle(t *testing.T, dir string) *moil.Bundle {
	t.Helper()
	b, err := moil.LoadBundle(os.DirFS(filepath.Join(vectorsDir, "bundles", dir)))
	if err != nil {
		t.Fatal(err)
	}
	return b
}

// loadBundle loads a bundle from spec/vectors/bundles and adds it to srv.
func loadBundle(t *testing.T, srv *moil.Server, dir string) *moil.Bundle {
	t.Helper()
	b := vectorBundle(t, dir)
	srv.AddBundle(b)
	return b
}

// submit submits a job and fails the test on error.
func submit(t *testing.T, srv *moil.Server, job moil.Job) *moil.Run {
	t.Helper()
	run, err := srv.Submit(context.Background(), job)
	if err != nil {
		t.Fatal(err)
	}
	return run
}

// wait waits for a run to end.
func wait(t *testing.T, run *moil.Run) (*moil.Result, error) {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	res, err := run.Wait(ctx)
	if ctx.Err() != nil {
		t.Fatalf("run %s didn't end: state %s", run.ID(), run.State())
	}
	return res, err
}

// succeeded waits for a run and fails the test unless it succeeded.
func succeeded(t *testing.T, run *moil.Run) *moil.Result {
	t.Helper()
	res, err := wait(t, run)
	if err != nil {
		t.Fatalf("run %s failed: %v", run.ID(), err)
	}
	return res
}

// events returns every event of a run that has ended.
func events(t *testing.T, run *moil.Run) []moil.Event {
	t.Helper()
	wait(t, run)
	ctx, cancel := context.WithTimeout(context.Background(), time.Second)
	defer cancel()
	return slices.Collect(run.Events(ctx))
}

// eventsSoFar returns the events a run has had until now.
func eventsSoFar(run *moil.Run) []moil.Event {
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	return slices.Collect(run.Events(ctx))
}

func kinds(evs []moil.Event) []moil.EventKind {
	out := make([]moil.EventKind, len(evs))
	for i, ev := range evs {
		out[i] = ev.Kind
	}
	return out
}

// eventually waits until cond holds, checking every few milliseconds.
func eventually(t *testing.T, what string, cond func() bool) {
	t.Helper()
	deadline := time.Now().Add(10 * time.Second)
	for !cond() {
		if time.Now().After(deadline) {
			t.Fatalf("timed out waiting for %s", what)
		}
		time.Sleep(2 * time.Millisecond)
	}
}

// httptestServer serves h for the test and returns its URL.
func httptestServer(t *testing.T, h http.Handler) string {
	ts := httptest.NewServer(h)
	t.Cleanup(ts.Close)
	return ts.URL
}

func machine(t *testing.T, srv *moil.Server, id string) moil.Machine {
	t.Helper()
	m, err := srv.Machine(context.Background(), id)
	if err != nil {
		t.Fatal(err)
	}
	return m
}

// fileServer stands in for object storage: it serves inputs and receives
// outputs, like pre-signed URLs would.
type fileServer struct {
	*httptest.Server
	mu    sync.Mutex
	files map[string][]byte
	types map[string]string
}

func newFileServer(t *testing.T, files map[string][]byte) *fileServer {
	fs := &fileServer{files: files, types: map[string]string{}}
	if fs.files == nil {
		fs.files = map[string][]byte{}
	}
	fs.Server = httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		fs.mu.Lock()
		defer fs.mu.Unlock()
		switch r.Method {
		case http.MethodGet:
			data, ok := fs.files[r.URL.Path]
			if !ok {
				http.NotFound(w, r)
				return
			}
			w.Write(data)
		case http.MethodPut, http.MethodPost:
			data, _ := io.ReadAll(r.Body)
			fs.files[r.URL.Path] = data
			fs.types[r.URL.Path] = r.Header.Get("Content-Type")
			w.WriteHeader(http.StatusCreated)
		}
	}))
	t.Cleanup(fs.Close)
	return fs
}

func (fs *fileServer) file(path string) ([]byte, string) {
	fs.mu.Lock()
	defer fs.mu.Unlock()
	return fs.files[path], fs.types[path]
}

func mustJSON(t *testing.T, v any) json.RawMessage {
	t.Helper()
	data, err := json.Marshal(v)
	if err != nil {
		t.Fatal(err)
	}
	return data
}
