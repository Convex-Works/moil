package e2e

import (
	"context"
	"crypto/rand"
	"errors"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"testing"
	"time"

	"git.convex.works/ConvexWorks/moil/sdk/go/moil"
)

// A service is a web service built on the moil SDK: the SDK's endpoints
// under /moil, and the object storage its jobs' inputs come from and
// outputs go to.
type service struct {
	t *testing.T
	// sdk is the service's moil.Server. Restart replaces it.
	sdk *moil.Server
	cfg moil.Config
	url string
	// link is the network machines reach the service through.
	link *link
	// probe is the probe bundle, which every service publishes.
	probe *moil.Bundle
	// record is where probe attempts, and the tests' hooks, record their
	// process groups.
	record string

	mu      sync.Mutex
	files   map[string]input   // inputs, by name
	uploads map[string]*upload // outputs, by name
}

// An input is a file the service's storage serves to whoever presents its
// token, as a pre-signed URL would.
type input struct {
	data  []byte
	token string
}

type serviceConfig struct {
	moil       moil.Config
	middleware func(http.Handler) http.Handler
}

type serviceOption func(*serviceConfig)

// withLease sets how long an attempt may go without word from its machine.
func withLease(d time.Duration) serviceOption {
	return func(c *serviceConfig) { c.moil.LeaseTTL = d }
}

// withMiddleware puts wrap in front of the SDK's endpoints, as a proxy in
// front of the service would be.
func withMiddleware(wrap func(http.Handler) http.Handler) serviceOption {
	return func(c *serviceConfig) { c.middleware = wrap }
}

func newService(t *testing.T, options ...serviceOption) *service {
	t.Helper()
	log := logFor(t)
	s := &service{
		t:       t,
		probe:   loadProbe(t),
		record:  t.TempDir(),
		files:   map[string]input{},
		uploads: map[string]*upload{},
	}
	c := serviceConfig{
		moil: moil.Config{
			Name:            "e2e",
			Store:           moil.NewMemoryStore(),
			Logger:          slog.New(slog.NewTextHandler(log.writer("service"), &slog.HandlerOptions{Level: slog.LevelDebug})),
			PairingInterval: time.Second,
			BidWindow:       time.Second,
			LeaseTTL:        10 * time.Second,
			PingInterval:    5 * time.Second,
		},
		middleware: func(h http.Handler) http.Handler { return h },
	}
	for _, option := range options {
		option(&c)
	}

	mux := http.NewServeMux()
	server := httptest.NewServer(mux)
	s.url = server.URL
	s.link = newLink(t, server.Listener.Addr().String())
	// Machines pair with the service through the link, so that's where
	// its confirmation page is (spec §4).
	c.moil.VerificationURL = s.link.URL() + "/pair"
	s.cfg = c.moil
	s.start()
	mux.Handle("/moil/", http.StripPrefix("/moil", c.middleware(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		s.mu.Lock()
		sdk := s.sdk
		s.mu.Unlock()
		sdk.Handler().ServeHTTP(w, r)
	}))))
	mux.HandleFunc("GET /files/{name}", s.serveFile)
	mux.HandleFunc("PUT /files/{name}", s.receiveFile)
	mux.HandleFunc("POST /files/{name}", s.receiveFile)

	t.Cleanup(func() {
		s.mu.Lock()
		sdk := s.sdk
		s.mu.Unlock()
		sdk.Close()
		s.link.close()
		server.Close()
		s.checkNothingOutlivedItsAgent()
	})
	return s
}

func (s *service) start() {
	sdk, err := moil.NewServer(s.cfg)
	if err != nil {
		s.t.Fatal(err)
	}
	sdk.AddBundle(s.probe)
	s.mu.Lock()
	s.sdk = sdk
	s.mu.Unlock()
}

// Restart restarts the SDK's server, as a deploy would: machines stay
// paired, jobs are forgotten.
func (s *service) Restart() {
	s.mu.Lock()
	old := s.sdk
	s.mu.Unlock()
	old.Close()
	s.start()
}

// Submit submits job, and fails the test if the SDK refuses it. Unless the
// job says otherwise it runs the probe, on any machine.
func (s *service) Submit(job moil.Job) *moil.Run {
	s.t.Helper()
	if job.Bundle == nil {
		job.Bundle = s.probe
	}
	if job.Eligible == nil {
		job.Eligible = moil.AnyMachine
	}
	if p, ok := job.Params.(probeParams); ok {
		p.Record = s.record
		job.Params = p
	}
	run, err := s.sdk.Submit(context.Background(), job)
	if err != nil {
		s.t.Fatalf("submitting a job: %v", err)
	}
	return run
}

// Machine returns the SDK's view of a machine.
func (s *service) Machine(id string) moil.Machine {
	s.t.Helper()
	m, err := s.sdk.Machine(context.Background(), id)
	if err != nil {
		s.t.Fatalf("machine %s: %v", id, err)
	}
	return m
}

// Serve stores data for machines to download as a job's input. The
// download needs the header that comes with it.
func (s *service) Serve(data []byte) moil.Download {
	s.mu.Lock()
	defer s.mu.Unlock()
	name := "input-" + strconv.Itoa(len(s.files)+1)
	token := rand.Text()
	s.files[name] = input{data: data, token: token}
	return moil.Download{URL: s.url + "/files/" + name, Headers: map[string]string{"Authorization": "Bearer " + token}}
}

// Receive makes a place for a machine to upload a job's output to.
func (s *service) Receive() *upload {
	s.mu.Lock()
	defer s.mu.Unlock()
	name := "output-" + strconv.Itoa(len(s.uploads)+1)
	u := &upload{Upload: moil.Upload{URL: s.url + "/files/" + name}}
	s.uploads[name] = u
	return u
}

func (s *service) serveFile(w http.ResponseWriter, r *http.Request) {
	s.mu.Lock()
	in, ok := s.files[r.PathValue("name")]
	s.mu.Unlock()
	if !ok {
		http.NotFound(w, r)
		return
	}
	if r.Header.Get("Authorization") != "Bearer "+in.token {
		http.Error(w, "the input's token is missing", http.StatusForbidden)
		return
	}
	w.Write(in.data)
}

func (s *service) receiveFile(w http.ResponseWriter, r *http.Request) {
	s.mu.Lock()
	u := s.uploads[r.PathValue("name")]
	s.mu.Unlock()
	if u == nil {
		http.NotFound(w, r)
		return
	}
	body, err := io.ReadAll(r.Body)
	if err != nil {
		http.Error(w, err.Error(), http.StatusBadRequest)
		return
	}
	u.mu.Lock()
	u.last, u.uploads = received{method: r.Method, header: r.Header, body: body}, u.uploads+1
	u.mu.Unlock()
	w.WriteHeader(http.StatusNoContent)
}

// An upload is where a job's output goes, and what arrived there.
type upload struct {
	moil.Upload
	mu      sync.Mutex
	last    received
	uploads int
}

// received is one upload as the service's storage received it.
type received struct {
	method string
	header http.Header
	body   []byte
}

// Received returns the last upload, and how many came.
func (u *upload) Received() (last received, uploads int) {
	u.mu.Lock()
	defer u.mu.Unlock()
	return u.last, u.uploads
}

// checkNothingOutlivedItsAgent fails the test if a process of a probe
// attempt or of a hook still runs once every machine has stopped, and
// kills it.
func (s *service) checkNothingOutlivedItsAgent() {
	entries, err := os.ReadDir(s.record)
	if err != nil {
		s.t.Error(err)
		return
	}
	for _, entry := range entries {
		what := "attempt " + entry.Name()
		if pid, ok := strings.CutPrefix(entry.Name(), "hook."); ok {
			what = "the hook run " + pid
		}
		data, err := os.ReadFile(filepath.Join(s.record, entry.Name()))
		pgid, _ := strconv.Atoi(strings.TrimSpace(string(data)))
		if err != nil || pgid <= 1 {
			s.t.Errorf("%s recorded no process group: %q, %v", what, data, err)
			continue
		}
		// Killed processes may take a moment to be reaped.
		deadline := time.Now().Add(2 * time.Second)
		for groupRuns(pgid) && time.Now().Before(deadline) {
			time.Sleep(10 * time.Millisecond)
		}
		if groupRuns(pgid) {
			if !s.t.Failed() {
				s.t.Errorf("processes of %s outlived the agent that ran them", what)
			}
			syscall.Kill(-pgid, syscall.SIGKILL)
		}
	}
}

// groupRuns reports whether a process of the group runs. On macOS a group
// left with only an unreaped leader answers EPERM; that counts as gone.
func groupRuns(pgid int) bool {
	err := syscall.Kill(-pgid, 0)
	return err == nil || !(errors.Is(err, syscall.ESRCH) || errors.Is(err, syscall.EPERM))
}
