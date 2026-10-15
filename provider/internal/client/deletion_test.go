package client

import "testing"

func TestAcksParseAsPorchPrintsThem(t *testing.T) {
	for in, want := range map[string]Ack{
		"workload.default#modules.web=delete":   {Descriptor: "workload.default#modules.web", Action: "delete"},
		"postgres.default#x=replace@sha256:abc": {Descriptor: "postgres.default#x", Action: "replace", Fingerprint: "sha256:abc"},
		"a=b=delete":                            {Descriptor: "a=b", Action: "delete"},
	} {
		got, err := ParseAck(in)
		if err != nil || got != want {
			t.Errorf("ParseAck(%q) = %+v, %v", in, got, err)
		}
		if got.String() != in {
			t.Errorf("%+v prints as %q, not %q", got, got.String(), in)
		}
	}
	for _, bad := range []string{"", "delete", "=delete", "x=", "x=delete@"} {
		if _, err := ParseAck(bad); err == nil {
			t.Errorf("ParseAck(%q) was accepted", bad)
		}
	}
}
