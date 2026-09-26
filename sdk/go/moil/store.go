package moil

import (
	"cmp"
	"context"
	"crypto/subtle"
	"encoding/json"
	"errors"
	"fmt"
	"maps"
	"os"
	"path/filepath"
	"slices"
	"sync"
)

// A MemoryStore is a Store that keeps machines in memory, for tests and
// demos. Pairings are lost when the process exits.
type MemoryStore struct {
	mu       sync.Mutex
	machines map[string]MachineRecord
	// save, if set, must persist machines before a change takes effect.
	save func(map[string]MachineRecord) error
}

// NewMemoryStore returns an empty MemoryStore.
func NewMemoryStore() *MemoryStore {
	return &MemoryStore{machines: map[string]MachineRecord{}}
}

// update applies f to a copy of the machines and keeps the copy only if f
// and save succeed, so a failed save changes nothing.
func (s *MemoryStore) update(f func(map[string]MachineRecord) error) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	next := maps.Clone(s.machines)
	if err := f(next); err != nil {
		return err
	}
	if s.save != nil {
		if err := s.save(next); err != nil {
			return err
		}
	}
	s.machines = next
	return nil
}

// AddMachine implements Store.
func (s *MemoryStore) AddMachine(_ context.Context, m MachineRecord) error {
	return s.update(func(machines map[string]MachineRecord) error {
		if _, ok := machines[m.ID]; ok {
			return fmt.Errorf("moil: machine %s already exists", m.ID)
		}
		machines[m.ID] = m.clone()
		return nil
	})
}

// Machine implements Store.
func (s *MemoryStore) Machine(_ context.Context, id string) (MachineRecord, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	m, ok := s.machines[id]
	if !ok {
		return MachineRecord{}, ErrNotFound
	}
	return m.clone(), nil
}

// MachineByToken implements Store.
func (s *MemoryStore) MachineByToken(_ context.Context, tokenHash string) (MachineRecord, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	for _, m := range s.machines {
		if subtle.ConstantTimeCompare([]byte(m.TokenHash), []byte(tokenHash)) == 1 {
			return m.clone(), nil
		}
	}
	return MachineRecord{}, ErrNotFound
}

// Machines implements Store.
func (s *MemoryStore) Machines(_ context.Context, owner string) ([]MachineRecord, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	var out []MachineRecord
	for _, m := range s.machines {
		if m.Owner == owner {
			out = append(out, m.clone())
		}
	}
	slices.SortFunc(out, func(a, b MachineRecord) int {
		return cmp.Or(a.PairedAt.Compare(b.PairedAt), cmp.Compare(a.ID, b.ID))
	})
	return out, nil
}

// SaveReport implements Store.
func (s *MemoryStore) SaveReport(_ context.Context, id string, r MachineReport) error {
	return s.update(func(machines map[string]MachineRecord) error {
		m, ok := machines[id]
		if !ok {
			return ErrNotFound
		}
		m.Report = r.clone()
		machines[id] = m
		return nil
	})
}

// RemoveMachine implements Store.
func (s *MemoryStore) RemoveMachine(_ context.Context, id string) error {
	return s.update(func(machines map[string]MachineRecord) error {
		if _, ok := machines[id]; !ok {
			return ErrNotFound
		}
		delete(machines, id)
		return nil
	})
}

// A FileStore is a MemoryStore that saves every change to a JSON file,
// replacing it atomically. It suits a single process with up to a few
// hundred machines; beyond that, implement Store over a database.
type FileStore struct {
	MemoryStore
	path string
}

type fileStoreData struct {
	Version  int             `json:"version"`
	Machines []MachineRecord `json:"machines"`
}

// NewFileStore opens the store saved at path, or starts an empty one if the
// file doesn't exist yet. The file and its directory are created on the
// first change, readable only by the current user.
func NewFileStore(path string) (*FileStore, error) {
	s := &FileStore{MemoryStore: MemoryStore{machines: map[string]MachineRecord{}}, path: path}
	s.save = s.write
	data, err := os.ReadFile(path)
	if errors.Is(err, os.ErrNotExist) {
		return s, nil
	}
	if err != nil {
		return nil, fmt.Errorf("moil: opening the machine store: %w", err)
	}
	var file fileStoreData
	if err := json.Unmarshal(data, &file); err != nil {
		return nil, fmt.Errorf("moil: the machine store %s is corrupt: %w", path, err)
	}
	if file.Version != 1 {
		return nil, fmt.Errorf("moil: the machine store %s has version %d; this SDK reads version 1", path, file.Version)
	}
	for _, m := range file.Machines {
		s.machines[m.ID] = m
	}
	return s, nil
}

func (s *FileStore) write(machines map[string]MachineRecord) error {
	file := fileStoreData{Version: 1, Machines: slices.Collect(maps.Values(machines))}
	slices.SortFunc(file.Machines, func(a, b MachineRecord) int { return cmp.Compare(a.ID, b.ID) })
	if file.Machines == nil {
		file.Machines = []MachineRecord{}
	}
	data, err := json.MarshalIndent(file, "", "  ")
	if err != nil {
		return err
	}
	if err := os.MkdirAll(filepath.Dir(s.path), 0o700); err != nil {
		return fmt.Errorf("moil: saving the machine store: %w", err)
	}
	tmp, err := os.CreateTemp(filepath.Dir(s.path), filepath.Base(s.path)+".*.tmp")
	if err != nil {
		return fmt.Errorf("moil: saving the machine store: %w", err)
	}
	defer os.Remove(tmp.Name())
	if _, err := tmp.Write(append(data, '\n')); err != nil {
		tmp.Close()
		return fmt.Errorf("moil: saving the machine store: %w", err)
	}
	if err := tmp.Sync(); err != nil {
		tmp.Close()
		return fmt.Errorf("moil: saving the machine store: %w", err)
	}
	if err := tmp.Close(); err != nil {
		return fmt.Errorf("moil: saving the machine store: %w", err)
	}
	if err := os.Rename(tmp.Name(), s.path); err != nil {
		return fmt.Errorf("moil: saving the machine store: %w", err)
	}
	return nil
}
