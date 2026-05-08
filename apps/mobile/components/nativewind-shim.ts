// @ts-nocheck
// Shim: provide a v3-compatible `styled` HOC on top of nativewind v4's cssInterop API.
// Gluestack v5 components reference `styled(Component, { className: 'style' })` (v3 API);
// nativewind v4 replaced that with `cssInterop(Component, { className: { target: 'style' } })`.
// This shim adapts the shape so vendored components run unchanged.

import { cssInterop } from 'nativewind';

type StyledMapping =
  | string
  | {
      target?: string | boolean;
      nativeStyleToProp?: Record<string, true | string>;
    };

export function styled<T extends React.ComponentType<any>>(
  Component: T,
  config?: { className?: StyledMapping } | StyledMapping,
): T {
  // Accept both `styled(C, { className: 'style' })` and `styled(C, 'style')`.
  let mapping: StyledMapping | undefined;
  if (config && typeof config === 'object' && 'className' in config) {
    mapping = config.className;
  } else if (config !== undefined) {
    mapping = config as StyledMapping;
  }

  let target: string | boolean = 'style';
  let nativeStyleToProp: Record<string, true | string> | undefined;
  if (typeof mapping === 'string') {
    target = mapping;
  } else if (mapping && typeof mapping === 'object') {
    if (mapping.target !== undefined) target = mapping.target;
    if (mapping.nativeStyleToProp) nativeStyleToProp = mapping.nativeStyleToProp;
  }

  cssInterop(Component, {
    className: nativeStyleToProp
      ? { target, nativeStyleToProp }
      : { target },
  });
  return Component;
}
