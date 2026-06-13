import React from 'react';
import type { VariantProps } from '@gluestack-ui/utils/nativewind-utils';
import { hstackStyle } from './styles';
import { pressToClick } from '../utils';

type IHStackProps = React.ComponentPropsWithoutRef<'div'> &
  VariantProps<typeof hstackStyle> & {
    onPress?: () => void;
  };

const HStack = React.forwardRef<React.ComponentRef<'div'>, IHStackProps>(
  function HStack(
    { className, space, reversed, onPress, onClick, ...props },
    ref
  ) {
    return (
      <div
        className={hstackStyle({
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

HStack.displayName = 'HStack';

export { HStack };
