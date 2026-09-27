package wire

import (
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"strconv"
)

// Limits of spec §10.
const (
	// MaxMessageBytes is the largest control message either side reads.
	MaxMessageBytes = 1 << 20
	// MaxPayloadBytes bounds a data event's payload and a result's meta,
	// serialized.
	MaxPayloadBytes = 256 << 10
	// MaxEventMessageBytes bounds the message of a progress or log event.
	MaxEventMessageBytes = 64 << 10
	// MaxStderrTailBytes bounds a failed attempt's stderr tail.
	MaxStderrTailBytes = 8 << 10
	// MaxDepth is how deeply arbitrary JSON (params, payload, meta) may
	// nest arrays and objects: [[1]] is two levels deep.
	MaxDepth = 64
)

// CheckValue fails unless a JSON value stays within what every peer can
// read (spec §2, §10): nested at most MaxDepth levels deep, with every
// number within the range of an IEEE 754 double. The error completes the
// sentence "the value …".
func CheckValue(raw []byte) error {
	dec := json.NewDecoder(bytes.NewReader(raw))
	dec.UseNumber()
	depth := 0
	for {
		tok, err := dec.Token()
		if err == io.EOF {
			return nil
		}
		if err != nil {
			return fmt.Errorf("isn't valid JSON: %v", err)
		}
		switch tok := tok.(type) {
		case json.Delim:
			switch tok {
			case '[', '{':
				if depth++; depth > MaxDepth {
					return fmt.Errorf("is nested more than %d levels deep", MaxDepth)
				}
			default:
				depth--
			}
		case json.Number:
			if _, err := strconv.ParseFloat(tok.String(), 64); err != nil {
				return fmt.Errorf("has the number %.40s, which is beyond the range of a double", tok)
			}
		}
	}
}
