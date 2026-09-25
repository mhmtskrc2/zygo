// SPDX-License-Identifier: Apache-2.0
// A Zygo warm-exec function in Go.
//
// No agent, no protocol: Zygo holds the sandbox warm and runs this program
// once per request, with the event as JSON on stdin and the result expected
// as JSON on stdout. A compiled binary starts in well under a millisecond, so
// there is nothing to amortise and an agent would only add a moving part.
// Anything written to stderr comes back to the caller as `stderr`; a non-zero
// exit is a failed request.
//
// Build it statically, so the image needs nothing:
//
//	CGO_ENABLED=0 go build -o parse .
//
// and point a function at it:
//
//	[fn.parse]
//	image = "alpine:3"
//	mounts = ["./parse:/app/parse:ro"]
//	cmd = ["/app/parse"]
package main

import (
	"encoding/json"
	"fmt"
	"os"
	"strings"
)

// Event is whatever the caller sent. Fields are optional on purpose: a
// warm-exec program is a function, and a function that refuses unknown
// input is a function nobody can call from a webhook.
type Event struct {
	Text string `json:"text"`
	N    int    `json:"n"`
}

// Result is the answer. `json:"-"` keeps nothing back; every field is the
// caller's to read.
type Result struct {
	Words    int    `json:"words"`
	Upper    string `json:"upper"`
	Squared  int    `json:"squared"`
	Language string `json:"language"`
}

func main() {
	var event Event
	if err := json.NewDecoder(os.Stdin).Decode(&event); err != nil {
		fmt.Fprintf(os.Stderr, "the event is not JSON: %v\n", err)
		os.Exit(1)
	}
	result := Result{
		Words:    len(strings.Fields(event.Text)),
		Upper:    strings.ToUpper(event.Text),
		Squared:  event.N * event.N,
		Language: "go",
	}
	if err := json.NewEncoder(os.Stdout).Encode(result); err != nil {
		fmt.Fprintf(os.Stderr, "could not write the result: %v\n", err)
		os.Exit(1)
	}
}
