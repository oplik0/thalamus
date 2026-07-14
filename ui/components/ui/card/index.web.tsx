import React from 'react';
import { cardStyle } from './styles';
import type { VariantProps } from '@gluestack-ui/utils/nativewind-utils';
import { pressToClick } from '../utils';

type ICardProps = React.ComponentPropsWithoutRef<'div'> &
  VariantProps<typeof cardStyle> & {
    onPress?: () => void;
  };

const Card = React.forwardRef<HTMLDivElement, ICardProps>(function Card(
  { className, size = 'md', variant = 'elevated', onPress, onClick, ...props },
  ref
) {
  return (
    <div
      className={cardStyle({ size, variant, class: className })}
      {...props}
      onClick={pressToClick(onPress, onClick)}
      ref={ref}
    />
  );
});

Card.displayName = 'Card';

export { Card };
