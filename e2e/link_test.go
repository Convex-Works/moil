package e2e

import (
	"bytes"
	"io"
	"net"
	"sync"
	"testing"
)

// A link is the network between a machine and its service: a TCP proxy
// that can be cut, and that remembers everything the service sent the
// machine.
type link struct {
	ln     net.Listener
	target string
	wg     sync.WaitGroup

	mu     sync.Mutex
	down   bool
	conns  map[net.Conn]bool
	opened int
	heard  []byte
}

// newLink starts a link to target (host:port). The machine that owns it
// closes it.
func newLink(t *testing.T, target string) *link {
	t.Helper()
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	l := &link{ln: ln, target: target, conns: map[net.Conn]bool{}}
	l.wg.Add(1)
	go l.accept()
	return l
}

// URL is where the machine reaches the service through the link.
func (l *link) URL() string { return "http://" + l.ln.Addr().String() }

// Cut drops every connection and refuses new ones until Restore.
func (l *link) Cut() {
	l.mu.Lock()
	defer l.mu.Unlock()
	l.down = true
	for c := range l.conns {
		c.Close()
	}
}

// Restore lets connections through again.
func (l *link) Restore() {
	l.mu.Lock()
	defer l.mu.Unlock()
	l.down = false
}

// Connections returns how many connections the machine opened through the
// link.
func (l *link) Connections() int {
	l.mu.Lock()
	defer l.mu.Unlock()
	return l.opened
}

// Heard reports whether the service ever sent the machine s. Control
// messages travel as plain WebSocket text frames, so a job's ID shows up
// here once the machine has been told anything about the job.
func (l *link) Heard(s string) bool {
	l.mu.Lock()
	defer l.mu.Unlock()
	return bytes.Contains(l.heard, []byte(s))
}

func (l *link) close() {
	l.ln.Close()
	l.Cut()
	l.wg.Wait()
}

func (l *link) accept() {
	defer l.wg.Done()
	for {
		machine, err := l.ln.Accept()
		if err != nil {
			return
		}
		l.mu.Lock()
		l.opened++
		down := l.down
		l.mu.Unlock()
		if down {
			machine.Close()
			continue
		}
		service, err := net.Dial("tcp", l.target)
		if err != nil {
			machine.Close()
			continue
		}
		if !l.track(machine, service) {
			continue
		}
		l.wg.Add(2)
		go l.pipe(service, machine, false)
		go l.pipe(machine, service, true)
	}
}

// track remembers a connection's two ends, so Cut can drop them. It
// reports false, having closed them, if the link was cut meanwhile.
func (l *link) track(ends ...net.Conn) bool {
	l.mu.Lock()
	defer l.mu.Unlock()
	for _, c := range ends {
		if l.down {
			c.Close()
			continue
		}
		l.conns[c] = true
	}
	return !l.down
}

// pipe copies from src to dst until either end closes, then closes both.
func (l *link) pipe(dst, src net.Conn, fromService bool) {
	defer l.wg.Done()
	var w io.Writer = dst
	if fromService {
		w = io.MultiWriter(dst, heardWriter{l})
	}
	io.Copy(w, src)
	dst.Close()
	src.Close()
	l.mu.Lock()
	delete(l.conns, dst)
	delete(l.conns, src)
	l.mu.Unlock()
}

type heardWriter struct{ l *link }

func (h heardWriter) Write(p []byte) (int, error) {
	h.l.mu.Lock()
	defer h.l.mu.Unlock()
	h.l.heard = append(h.l.heard, p...)
	return len(p), nil
}
