// @ts-nocheck
import React from 'react';
import { boxStyle } from './styles';

import type { VariantProps } from '@gluestack-ui/utils/nativewind-utils';

type IBoxProps = React.ComponentPropsWithoutRef<'div'> &
  VariantProps<typeof boxStyle> & { className?: string };

const Box = React.forwardRef<HTMLDivElement, IBoxProps>(function Box(
  { className, style, ...props },
  ref
) {
  // RN-Web list internals (VirtualizedList, etc.) clone children with
  // `style: [...]` — a plain <div> can't accept an array. Flatten so Box is
  // safe to use inside RN-Web lists.
  const flatStyle = Array.isArray(style) ? Object.assign({}, ...style.flat(Infinity).filter(Boolean)) : style;
  return (
    <div ref={ref} className={boxStyle({ class: className })} {...props} style={flatStyle} />
  );
});

Box.displayName = 'Box';
export { Box };
