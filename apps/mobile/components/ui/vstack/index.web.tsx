// @ts-nocheck
import React from 'react';
import type { VariantProps } from '@gluestack-ui/utils/nativewind-utils';

import { vstackStyle } from './styles';

type IVStackProps = React.ComponentProps<'div'> &
  VariantProps<typeof vstackStyle>;

const VStack = React.forwardRef<React.ComponentRef<'div'>, IVStackProps>(
  function VStack({ className, space, reversed, style, ...props }, ref) {
    // RN-Web's VirtualizedList clones ListEmptyComponent / list children with
    // `style: [oldStyle, layoutStyle]`. A plain <div> can't accept an array
    // style — React DOM tries setValueForStyles with numeric keys and crashes.
    // Flatten arrays here so this component is safe to use inside RN-Web lists.
    const flatStyle = Array.isArray(style) ? Object.assign({}, ...style.flat(Infinity).filter(Boolean)) : style;
    return (
      <div
        className={vstackStyle({
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

VStack.displayName = 'VStack';

export { VStack };
