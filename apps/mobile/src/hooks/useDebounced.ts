import { useState, useEffect } from 'react';

/**
 * Returns a debounced copy of `value` that only updates after `delayMs`
 * milliseconds have elapsed since the last change.
 *
 * @param value    The value to debounce.
 * @param delayMs  Debounce delay in milliseconds (default: 200).
 *
 * @example
 *   const debouncedQuery = useDebounced(query, 200);
 */
export function useDebounced<T>(value: T, delayMs = 200): T {
  const [debounced, setDebounced] = useState<T>(value);

  useEffect(() => {
    const id = setTimeout(() => setDebounced(value), delayMs);
    return () => clearTimeout(id);
  }, [value, delayMs]);

  return debounced;
}
