import type { MouseEventHandler } from 'react';

/**
 * Convert a React Native `onPress` callback into a web `onClick` handler.
 *
 * This lets cross-platform components keep using `onPress` while avoiding
 * React DOM warnings about unknown event handler properties when the same
 * component is rendered on the web.
 */
export function pressToClick<T extends Element>(
  onPress?: () => void,
  onClick?: MouseEventHandler<T>
): MouseEventHandler<T> | undefined {
  if (!onPress) return onClick;
  return (event) => {
    onClick?.(event);
    onPress();
  };
}
