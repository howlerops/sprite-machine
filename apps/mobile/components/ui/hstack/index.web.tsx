// @ts-nocheck
import React from 'react';
import type { VariantProps } from '@gluestack-ui/utils/nativewind-utils';
import { hstackStyle } from './styles';

type IHStackProps = React.ComponentPropsWithoutRef<'div'> &
  VariantProps<typeof hstackStyle>;

const HStack = React.forwardRef<React.ComponentRef<'div'>, IHStackProps>(
  function HStack({ className, space, reversed, style, ...props }, ref) {
    // See vstack/index.web.tsx — RN-Web list cloneElement passes `style` as an
    // array; plain <div> rejects it. Flatten so this component is list-safe.
    const flatStyle = Array.isArray(style) ? Object.assign({}, ...style.flat(Infinity).filter(Boolean)) : style;
    return (
      <div
        className={hstackStyle({
          space,
          reversed: reversed as boolean,
          class: className,
        })}
        {...props}
        style={flatStyle}
        ref={ref}
      />
    );
  }
);

HStack.displayName = 'HStack';

export { HStack };
