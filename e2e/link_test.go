package e2e

import (
	"bytes"
	"io"
	"net"
	"sync"
	"testing"
)

// A link is the network in front of a service, which its machines reach
// it through: a TCP proxy that can be cut, and that remembers what went
// over each connection.
type link struct {
	ln     net.Listener
	target string
	wg     sync.WaitGroup

	mu      sync.Mutex
	down    bool
	conns   map[net.Conn]bool
	opened  int
	traffic []*traffic // each connection's, oldest first
}

// traffic is what went over one connection: what the machine sent, and
// what the service sent back.
type traffic struct{ sent, heard []byte }

// newLink starts a link to target (host:port). The service that owns it
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

// URL is where machines reach the service through the link.
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

// Connections returns how many connections machines opened through the
// link.
func (l *link) Connections() int {
	l.mu.Lock()
	defer l.mu.Unlock()
	return l.opened
}

// Heard reports whether the service ever sent s over a connection that
// carried token, as every request of the machine holding it does.
// Control messages travel as plain WebSocket text frames, so a job's ID
// shows up here once the machine has been told anything about the job.
func (l *link) Heard(token, s string) bool {
	l.mu.Lock()
	defer l.mu.Unlock()
	for _, t := range l.traffic {
		if bytes.Contains(t.sent, []byte(token)) && bytes.Contains(t.heard, []byte(s)) {
			return true
		}
	}
	return false
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
		t := &traffic{}
		if !l.track(t, machine, service) {
			continue
		}
		l.wg.Add(2)
		go l.pipe(service, machine, &t.sent)
		go l.pipe(machine, service, &t.heard)
	}
}

// track remembers a connection's two ends, so Cut can drop them, and its
// traffic. It reports false, having closed them, if the link was cut
// meanwhile.
func (l *link) track(t *traffic, ends ...net.Conn) bool {
	l.mu.Lock()
	defer l.mu.Unlock()
	for _, c := range ends {
		if l.down {
			c.Close()
			continue
		}
		l.conns[c] = true
	}
	if !l.down {
		l.traffic = append(l.traffic, t)
	}
	return !l.down
}

// pipe copies from src to dst, and to record, until either end closes,
// then closes both.
func (l *link) pipe(dst, src net.Conn, record *[]byte) {
	defer l.wg.Done()
	io.Copy(io.MultiWriter(dst, recorder{l, record}), src)
	dst.Close()
	src.Close()
	l.mu.Lock()
	delete(l.conns, dst)
	delete(l.conns, src)
	l.mu.Unlock()
}

// A recorder appends what it's written to one direction of a connection's
// traffic.
type recorder struct {
	l  *link
	to *[]byte
}

func (r recorder) Write(p []byte) (int, error) {
	r.l.mu.Lock()
	defer r.l.mu.Unlock()
	*r.to = append(*r.to, p...)
	return len(p), nil
}
