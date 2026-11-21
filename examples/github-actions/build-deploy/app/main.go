// An HTTP service that answers with the version it was built as.
package main

import (
	"fmt"
	"log"
	"net/http"
)

var version = "dev"

func main() {
	http.HandleFunc("/health", func(w http.ResponseWriter, _ *http.Request) {
		fmt.Fprintln(w, "ok")
	})
	http.HandleFunc("/", func(w http.ResponseWriter, _ *http.Request) {
		fmt.Fprintf(w, "version: %s\n", version)
	})
	log.Fatal(http.ListenAndServe(":8080", nil))
}
