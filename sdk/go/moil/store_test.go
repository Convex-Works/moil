package moil_test

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"reflect"
	"slices"
	"strings"
	"sync"
	"testing"
	"time"

	"git.convex.works/ConvexWorks/moil/sdk/go/moil"
	"git.convex.works/ConvexWorks/moil/sdk/go/moiltest"
)

func TestMemoryStore(t *testing.T) {
	moiltest.TestStore(t, func(*testing.T) moil.Store { return moil.NewMemoryStore() })
}

func TestFileStore(t *testing.T) {
	moiltest.TestStore(t, func(t *testing.T) moil.Store {
		s, err := moil.NewFileStore(filepath.Join(t.TempDir(), "moil", "machines.json"))
		if err != nil {
			t.Fatal(err)
		}
		return s
	})
}

// A FileStore reopened from its file holds what was saved, and the file is
// private to the service's user.
func TestFileStorePersists(t *testing.T) {
	path := filepath.Join(t.TempDir(), "state", "machines.json")
	s, err := moil.NewFileStore(path)
	if err != nil {
		t.Fatal(err)
	}
	ctx := context.Background()
	rec := moil.MachineRecord{ID: "m_1", Owner: "alice", TokenHash: strings.Repeat("ab", 32), PairedAt: time.Now().UTC()}
	if err := s.AddMachine(ctx, rec); err != nil {
		t.Fatal(err)
	}
	if err := s.SaveReport(ctx, "m_1", moil.MachineReport{Name: "desk", GPUs: []moil.GPU{{Name: "RTX"}}}); err != nil {
		t.Fatal(err)
	}
	info, err := os.Stat(path)
	if err != nil {
		t.Fatal(err)
	}
	if info.Mode().Perm()&0o077 != 0 {
		t.Errorf("the store file is %v; others must not read it", info.Mode().Perm())
	}

	again, err := moil.NewFileStore(path)
	if err != nil {
		t.Fatal(err)
	}
	got, err := again.MachineByToken(ctx, rec.TokenHash)
	if err != nil || got.Owner != "alice" || got.Report.Name != "desk" || len(got.Report.GPUs) != 1 || !got.PairedAt.Equal(rec.PairedAt) {
		t.Fatalf("reopened: %+v, %v", got, err)
	}
}

func TestFileStoreRefusesACorruptFile(t *testing.T) {
	dir := t.TempDir()
	for name, content := range map[string]string{
		"garbage.json": "{not json",
		"future.json":  `{"version": 2, "machines": []}`,
	} {
		path := filepath.Join(dir, name)
		if err := os.WriteFile(path, []byte(content), 0o600); err != nil {
			t.Fatal(err)
		}
		if _, err := moil.NewFileStore(path); err == nil {
			t.Errorf("%s: opened a store it can't read; saving would lose the machines in it", name)
		}
	}
}

// slowStore holds every SaveReport until released, and counts them.
type slowStore struct {
	moil.Store
	release chan struct{}
	mu      sync.Mutex
	saves   int
}

func (s *slowStore) SaveReport(ctx context.Context, id string, r moil.MachineReport) error {
	select {
	case <-s.release:
	case <-ctx.Done():
		return ctx.Err()
	}
	s.mu.Lock()
	s.saves++
	s.mu.Unlock()
	return s.Store.SaveReport(ctx, id, r)
}

// A machine that reports faster than the Store saves can't make the
// Server hold more than its latest report, and Close doesn't wait to save
// the ones it replaced.
func TestOnlyTheLatestReportWaitsForTheStore(t *testing.T) {
	store := &slowStore{Store: moil.NewMemoryStore(), release: make(chan struct{})}
	srv := newServer(t, func(c *moil.Config) { c.Store = store })
	bundle := loadBundle(t, srv, "minimal")
	m := moiltest.Pair(t, srv, "alice")
	m.Connect() // its hello waits for the Store
	for range 200 {
		m.Approve(bundle)
		m.Revoke(bundle)
	}
	m.Approve(bundle)
	m.Sync()
	close(store.release)
	eventually(t, "the last report to be saved", func() bool {
		rec, err := store.Machine(context.Background(), m.ID())
		return err == nil && slices.Equal(rec.Report.Approved, []string{bundle.Hash()})
	})
	start := time.Now()
	srv.Close()
	if took := time.Since(start); took > time.Second {
		t.Errorf("Close took %v", took)
	}
	store.mu.Lock()
	defer store.mu.Unlock()
	if store.saves > 3 {
		t.Errorf("the Store saved %d reports of 402; only the latest should wait", store.saves)
	}
}

// A FileStore reads files in the format it has always written, which is
// its own: the public types don't decide it.
func TestFileStoreFormat(t *testing.T) {
	path := filepath.Join(t.TempDir(), "machines.json")
	saved := `{
  "version": 1,
  "machines": [
    {
      "id": "m_1",
      "owner": "alice",
      "token_hash": "` + strings.Repeat("ab", 32) + `",
      "paired_at": "2026-09-26T10:00:00Z",
      "report": {
        "name": "Ada's desktop", "os": "linux", "arch": "x86_64", "app_version": "0.1.0",
        "cpus": 16, "memory_bytes": 68719476736,
        "gpus": [{"name": "RTX 4090", "memory_bytes": 25769803776}],
        "approved": ["` + strings.Repeat("a", 64) + `"],
        "last_seen": "2026-09-26T11:00:00Z"
      }
    }
  ]
}
`
	if err := os.WriteFile(path, []byte(saved), 0o600); err != nil {
		t.Fatal(err)
	}
	s, err := moil.NewFileStore(path)
	if err != nil {
		t.Fatal(err)
	}
	got, err := s.Machine(context.Background(), "m_1")
	if err != nil {
		t.Fatal(err)
	}
	r := got.Report
	if got.Owner != "alice" || got.PairedAt.Hour() != 10 || r.Name != "Ada's desktop" || r.CPUs != 16 ||
		!slices.Equal(r.GPUs, []moil.GPU{{Name: "RTX 4090", MemoryBytes: 24 << 30}}) || len(r.Approved) != 1 || r.LastSeen.Hour() != 11 {
		t.Fatalf("read %+v", got)
	}
	// Writing it back keeps the format.
	if err := s.SaveReport(context.Background(), "m_1", r); err != nil {
		t.Fatal(err)
	}
	written, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	var want, have any
	if json.Unmarshal([]byte(saved), &want) != nil || json.Unmarshal(written, &have) != nil || !reflect.DeepEqual(want, have) {
		t.Fatalf("rewrote the file as\n%s", written)
	}
}

// A Machine marshals with one naming throughout, the Go field names, so a
// service can hand it to its own frontend.
func TestMachineMarshalsConsistently(t *testing.T) {
	data, err := json.Marshal(moil.Machine{ID: "m_1", MachineReport: moil.MachineReport{Name: "desk", GPUs: []moil.GPU{{Name: "RTX"}}}, State: moil.Idle})
	if err != nil {
		t.Fatal(err)
	}
	var fields map[string]json.RawMessage
	if err := json.Unmarshal(data, &fields); err != nil {
		t.Fatal(err)
	}
	for _, key := range []string{"ID", "Owner", "PairedAt", "Name", "OS", "Arch", "AppVersion", "CPUs", "MemoryBytes", "GPUs", "Approved", "LastSeen", "State"} {
		if _, ok := fields[key]; !ok {
			t.Errorf("%s is missing from %s", key, data)
		}
	}
	if len(fields) != 13 {
		t.Errorf("unexpected fields in %s", data)
	}
}
