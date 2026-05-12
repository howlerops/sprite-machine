// Hermes compatibility shim for a transitive dependency that references the
// global `_toString` helper in Metro dev bundles.
globalThis._toString = Object.prototype.toString;

require('expo-router/entry');
