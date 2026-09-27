package wire

import (
	"strings"
	"testing"
)

func TestCheckValue(t *testing.T) {
	deep := func(n int) string { return strings.Repeat("[", n) + strings.Repeat("]", n) }
	for _, c := range []struct {
		value string
		ok    bool
	}{
		{`null`, true},
		{deep(MaxDepth), true},
		{deep(MaxDepth + 1), false},
		{`{"a": ` + deep(MaxDepth-1) + `}`, true},
		{`{"a": ` + deep(MaxDepth) + `}`, false},
		{`[1.7976931348623157e308, -1e-400, 18446744073709551616]`, true},
		{`{"n": 1e400}`, false},
		{`[-2e308]`, false},
	} {
		if err := CheckValue([]byte(c.value)); (err == nil) != c.ok {
			t.Errorf("CheckValue(%.60s) = %v, want ok %v", c.value, err, c.ok)
		}
	}
}
