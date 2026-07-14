import React from 'react';
import type { VariantProps } from '@gluestack-ui/utils/nativewind-utils';

import { vstackStyle } from './styles';
import { pressToClick } from '../utils';

type IVStackProps = React.ComponentPropsWithoutRef<'div'> &
  VariantProps<typeof vstackStyle> & {
    onPress?: () => void;
  };

const VStack = React.forwardRef<HTMLDivElement, IVStackProps>(
  function VStack(
    { className, space, reversed, onPress, onClick, ...props },
    ref
  ) {
    return (
      <div
        className={vstackStyle({
          space,
          reversed: reversed as boolean,
          class: className,
        })}
        {...props}
        onClick={pressToClick(onPress, onClick)}
        ref={ref}
      />
    );
  }
);

VStack.displayName = 'VStack';

export { VStack };
