package moil

import (
	"context"
	"log/slog"
	"testing"
	"time"
)

// hungStore never saves a report, until the context ends.
type hungStore struct{ Store }

func (hungStore) SaveReport(ctx context.Context, _ string, _ MachineReport) error {
	<-ctx.Done()
	return ctx.Err()
}

// Closing gives a hung Store the grace it's due, and no more, however
// many reports wait.
func TestReporterCloseIsBounded(t *testing.T) {
	r := newReporter(hungStore{}, slog.New(slog.DiscardHandler))
	for _, id := range []string{"m_1", "m_2", "m_3"} {
		r.save(id, MachineReport{Name: id})
	}
	start := time.Now()
	r.close(50 * time.Millisecond)
	if took := time.Since(start); took > time.Second {
		t.Fatalf("close took %v with a grace of 50ms", took)
	}
	r.save("m_4", MachineReport{}) // after close: dropped, not blocking
}
