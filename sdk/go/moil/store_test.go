package moil_test

import (
	"context"
	"os"
	"path/filepath"
	"strings"
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
