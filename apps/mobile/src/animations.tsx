/**
 * Reusable animation primitives for HowlerForge.
 * See `.omc/research/animation-spec.md` for the canonical spec.
 *
 * All animations gate on `useReduceMotion()`; when reduce-motion is on the
 * animations skip to their final state instantly.
 */
import { useEffect, useRef, useState, type ReactNode } from 'react';
import { AccessibilityInfo, Platform } from 'react-native';
import Animated, {
  Easing,
  cancelAnimation,
  useAnimatedStyle,
  useSharedValue,
  withDelay,
  withRepeat,
  withTiming,
} from 'react-native-reanimated';

/**
 * Tracks the OS reduce-motion preference. Returns `true` when motion should
 * be minimised. Falls back to `false` if the platform doesn't expose the
 * setting (web in some browsers).
 */
export function useReduceMotion(): boolean {
  const [reduce, setReduce] = useState(false);

  useEffect(() => {
    let mounted = true;
    AccessibilityInfo.isReduceMotionEnabled?.()
      .then((v) => {
        if (mounted) setReduce(Boolean(v));
      })
      .catch(() => {
        // ignore — default to false
      });
    const sub = AccessibilityInfo.addEventListener?.('reduceMotionChanged', (v) => {
      if (mounted) setReduce(Boolean(v));
    });
    return () => {
      mounted = false;
      sub?.remove?.();
    };
  }, []);

  return reduce;
}

/**
 * Breathing pulse for the "running" status badge. Returns a Reanimated style
 * object suitable for `<Animated.View style={...}>`. The pulse loops while
 * `active` is true; on stop / reduce-motion the opacity snaps back to 1.
 *
 * Spec: opacity 1 → 0.55 → 1, 1400ms, Easing.inOut(Easing.sin).
 */
export function usePulse(active: boolean) {
  const opacity = useSharedValue(1);
  const reduceMotion = useReduceMotion();

  useEffect(() => {
    if (!active || reduceMotion) {
      cancelAnimation(opacity);
      opacity.value = 1;
      return;
    }
    opacity.value = withRepeat(
      withTiming(0.55, { duration: 700, easing: Easing.inOut(Easing.sin) }),
      -1,
      true,
    );
    return () => {
      cancelAnimation(opacity);
      opacity.value = 1;
    };
  }, [active, reduceMotion, opacity]);

  return useAnimatedStyle(() => ({ opacity: opacity.value }));
}

/**
 * Mount-time stagger entrance: opacity 0 → 1, translateY 8 → 0, 220ms,
 * Easing.out(Easing.cubic). Per-instance delay caps at index 8 * 35ms so
 * long lists don't cascade forever (spec §3).
 */
export function StaggerEnter({ delay = 0, children }: { delay?: number; children: ReactNode }) {
  const opacity = useSharedValue(0);
  const translateY = useSharedValue(8);
  const reduceMotion = useReduceMotion();
  const startedRef = useRef(false);

  useEffect(() => {
    if (startedRef.current) return;
    startedRef.current = true;

    if (reduceMotion) {
      opacity.value = 1;
      translateY.value = 0;
      return;
    }

    const cappedDelay = Math.min(delay, 8 * 35);
    opacity.value = withDelay(
      cappedDelay,
      withTiming(1, { duration: 220, easing: Easing.out(Easing.cubic) }),
    );
    translateY.value = withDelay(
      cappedDelay,
      withTiming(0, { duration: 220, easing: Easing.out(Easing.cubic) }),
    );
  }, [delay, reduceMotion, opacity, translateY]);

  // Web: opacity-only to avoid RN-Web's "indexed property setter" bug with
  // Reanimated transform arrays. Native keeps the 8→0 px rise.
  const style = useAnimatedStyle(() => {
    if (Platform.OS === 'web') return { opacity: opacity.value };
    return {
      opacity: opacity.value,
      transform: [{ translateY: translateY.value }],
    };
  });

  return <Animated.View style={style}>{children}</Animated.View>;
}

/**
 * Cross-dissolve for badge label/colour transitions on status change.
 * Renders the new content stacked over the old; new fades in (260ms,
 * Easing.inOut(Easing.quad)) while old fades out. First mount is instant
 * (no diff to animate).
 */
export function CrossfadePill({
  contentKey,
  children,
}: {
  contentKey: string;
  children: ReactNode;
}) {
  const opacity = useSharedValue(1);
  const reduceMotion = useReduceMotion();
  const firstMountRef = useRef(true);
  const lastKeyRef = useRef(contentKey);

  useEffect(() => {
    if (firstMountRef.current) {
      firstMountRef.current = false;
      lastKeyRef.current = contentKey;
      return;
    }
    if (lastKeyRef.current === contentKey) return;
    lastKeyRef.current = contentKey;
    if (reduceMotion) {
      opacity.value = 1;
      return;
    }
    opacity.value = 0;
    opacity.value = withTiming(1, { duration: 260, easing: Easing.inOut(Easing.quad) });
  }, [contentKey, reduceMotion, opacity]);

  const style = useAnimatedStyle(() => ({ opacity: opacity.value }));
  return <Animated.View style={style}>{children}</Animated.View>;
}

/**
 * Cross-dissolve container for tab swaps inside a screen (Timeline ↔ Shell).
 * Outgoing fades 120ms, incoming fades 180ms with a 4px translateY rise.
 * Reduce-motion → instant swap.
 */
export function TabCrossfade({
  contentKey,
  children,
}: {
  contentKey: string;
  children: ReactNode;
}) {
  const opacity = useSharedValue(1);
  const translateY = useSharedValue(0);
  const reduceMotion = useReduceMotion();
  const lastKeyRef = useRef(contentKey);

  useEffect(() => {
    if (lastKeyRef.current === contentKey) return;
    lastKeyRef.current = contentKey;
    if (reduceMotion) {
      opacity.value = 1;
      translateY.value = 0;
      return;
    }
    opacity.value = 0;
    translateY.value = 4;
    opacity.value = withTiming(1, { duration: 180, easing: Easing.out(Easing.cubic) });
    translateY.value = withTiming(0, { duration: 180, easing: Easing.out(Easing.cubic) });
  }, [contentKey, reduceMotion, opacity, translateY]);

  // RN-Web has a bug where Reanimated `transform: [{translateY}]` inside an
  // animated style causes "Failed to set indexed property [0] on
  // CSSStyleDeclaration" during commit. Use opacity-only on web; native still
  // gets the slight rise via the platform-aware ternary below.
  const style = useAnimatedStyle(() => {
    if (Platform.OS === 'web') return { opacity: opacity.value, flex: 1 };
    return {
      opacity: opacity.value,
      transform: [{ translateY: translateY.value }],
      flex: 1,
    };
  });

  return <Animated.View style={style}>{children}</Animated.View>;
}

/**
 * Typewriter reveal for the first N characters of a string. After the limit
 * is reached, the parent should switch back to rendering raw output.
 * Reduce-motion → returns the full text immediately.
 *
 * Spec §10: 18ms/char for first 30 chars, no animation on reconnect.
 */
export function useTypewriter(
  source: string,
  options: { active: boolean; limit?: number; intervalMs?: number } = { active: true },
): { text: string; done: boolean } {
  const limit = options.limit ?? 30;
  const intervalMs = options.intervalMs ?? 18;
  const reduceMotion = useReduceMotion();
  const [n, setN] = useState(0);
  const startedRef = useRef(false);

  useEffect(() => {
    if (!options.active) return;
    if (startedRef.current) return;
    startedRef.current = true;

    if (reduceMotion || Platform.OS !== 'web') {
      // Native shell isn't rendered today; web is the only consumer. Still
      // skip on RM regardless.
      setN(Math.min(source.length, limit));
      return;
    }

    let i = 0;
    const id = setInterval(() => {
      i += 1;
      setN(i);
      if (i >= limit) clearInterval(id);
    }, intervalMs);
    return () => clearInterval(id);
  }, [options.active, reduceMotion, source.length, limit, intervalMs]);

  const capped = Math.min(n, source.length, limit);
  return { text: source.slice(0, capped), done: capped >= Math.min(source.length, limit) };
}
