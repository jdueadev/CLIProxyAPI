//go:build sealed

package configaccess

// sealed is true in a build made with -tags sealed: the binary has no open mode. With no client keys the
// provider stays registered and refuses every request, however the program was started.
const sealed = true
