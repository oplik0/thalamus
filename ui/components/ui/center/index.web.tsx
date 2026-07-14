import React from 'react';
import { centerStyle } from './styles';

import type { VariantProps } from '@gluestack-ui/utils/nativewind-utils';
import { pressToClick } from '../utils';

type ICenterProps = React.ComponentPropsWithoutRef<'div'> &
  VariantProps<typeof centerStyle> & {
    onPress?: () => void;
  };

const Center = React.forwardRef<HTMLDivElement, ICenterProps>(function Center(
  { className, onPress, onClick, ...props },
  ref
) {
  return (
    <div
      className={centerStyle({ class: className })}
      {...props}
      onClick={pressToClick(onPress, onClick)}
      ref={ref}
    />
  );
});

Center.displayName = 'Center';

export { Center };
