// Disable autolinking for react-native-worklets on native platforms.
// We only need this package for the babel plugin (loaded by
// react-native-css-interop / nativewind) when bundling for web. The 0.8.x
// native module is incompatible with RN 0.79.5; Reanimated 3.x ships its
// own worklets runtime internally, so disabling autolinking is safe.
module.exports = {
  dependencies: {
    'react-native-worklets': {
      platforms: { ios: null, android: null },
    },
  },
};
