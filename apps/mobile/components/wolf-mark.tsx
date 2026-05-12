import { Image } from 'react-native';

/**
 * HowlerForge gold-gradient wolf head used as the brand mark in headers and
 * the desktop rail. The asset is shared with the app icon (assets/icon.png),
 * so it's already cached after first paint.
 */
export function WolfMark({ size = 24 }: { size?: number }) {
  return (
    <Image
      source={require('../assets/icon.png')}
      style={{ width: size, height: size }}
      resizeMode="contain"
      accessibilityLabel="HowlerForge"
    />
  );
}
