import React from 'react';
import { boxStyle } from './styles';

import type { VariantProps } from '@gluestack-ui/utils/nativewind-utils';
import { pressToClick } from '../utils';

type IBoxProps = React.ComponentPropsWithoutRef<'div'> &
  VariantProps<typeof boxStyle> & {
    className?: string;
    onPress?: () => void;
  };

const Box = React.forwardRef<HTMLDivElement, IBoxProps>(function Box(
  { className, onPress, onClick, ...props },
  ref
) {
  return (
    <div
      ref={ref}
      className={boxStyle({ class: className })}
      {...props}
      onClick={pressToClick(onPress, onClick)}
    />
  );
});

Box.displayName = 'Box';
export { Box };
