// Loads cleanly but exports nothing usable as a provider or factory — the case
// `provider-host.mts` reports by naming what it *did* find, rather than a bare
// "no provider" that would leave a typo in `spec.export` (or here, a module that
// simply doesn't export what's expected) to be guessed at from nothing.

export const somethingElse = 42;
