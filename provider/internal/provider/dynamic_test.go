package provider

import (
	"context"
	"encoding/json"
	"strings"
	"testing"
)

func TestJSONRoundTripsThroughDynamicValues(t *testing.T) {
	ctx := context.Background()
	in := `{"role_arn":"arn:aws:iam::111122223333:role/x","session_duration":3600,"ratio":0.5,` +
		`"enabled":true,"tags":["a","b"],"nested":{"empty":null,"list":[1,{"k":"v"}]}}`
	dec := json.NewDecoder(strings.NewReader(in))
	dec.UseNumber()
	var x any
	if err := dec.Decode(&x); err != nil {
		t.Fatal(err)
	}
	d, err := jsonToDynamic(ctx, x)
	if err != nil {
		t.Fatal(err)
	}
	back, err := dynamicToJSON(ctx, d)
	if err != nil {
		t.Fatal(err)
	}
	if !sameJSON(x, back) {
		t.Fatalf("round trip changed the value:\n%v\n%v", x, back)
	}
	out, err := json.Marshal(back)
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(string(out), `"session_duration":3600`) {
		t.Fatalf("numbers are not kept as written: %s", out)
	}
}
