package moil

import "sync"

// A queue is a bounded FIFO: push never blocks, so the scheduler can hand
// work to slower goroutines without waiting for them.
type queue[T any] struct {
	mu     sync.Mutex
	items  []T
	limit  int
	closed bool
	ready  chan struct{} // holds a token while items isn't empty or the queue is closed
}

// newQueue returns a queue that holds at most limit items.
func newQueue[T any](limit int) *queue[T] {
	return &queue[T]{limit: limit, ready: make(chan struct{}, 1)}
}

// push adds v. It reports false, dropping v, if the queue is full or
// closed.
func (q *queue[T]) push(v T) bool {
	q.mu.Lock()
	defer q.mu.Unlock()
	if q.closed || len(q.items) >= q.limit {
		return false
	}
	q.items = append(q.items, v)
	q.signal()
	return true
}

// close stops new pushes. Items already queued can still be popped.
func (q *queue[T]) close() {
	q.mu.Lock()
	defer q.mu.Unlock()
	q.closed = true
	q.signal()
}

func (q *queue[T]) signal() {
	select {
	case q.ready <- struct{}{}:
	default:
	}
}

// pop waits for the next item. It reports false once the queue is closed
// and empty, or when done is closed.
func (q *queue[T]) pop(done <-chan struct{}) (T, bool) {
	for {
		q.mu.Lock()
		if len(q.items) > 0 {
			v := q.items[0]
			var zero T
			q.items[0] = zero
			q.items = q.items[1:]
			if len(q.items) > 0 {
				q.signal()
			}
			q.mu.Unlock()
			return v, true
		}
		closed := q.closed
		q.mu.Unlock()
		if closed {
			var zero T
			return zero, false
		}
		select {
		case <-q.ready:
		case <-done:
			var zero T
			return zero, false
		}
	}
}
