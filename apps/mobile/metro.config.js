const { getDefaultConfig } = require('expo/metro-config');
const { withNativeWind } = require('nativewind/metro');
const path = require('node:path');

const projectRoot = __dirname;
const workspaceRoot = path.resolve(projectRoot, '../..');

const config = getDefaultConfig(projectRoot);

// Watch all packages in the monorepo while preserving Expo SDK defaults.
config.watchFolders = Array.from(new Set([...(config.watchFolders ?? []), workspaceRoot]));

// Allow Metro to resolve modules from both the project and workspace roots,
// with the project-local node_modules listed first so it wins for packages
// that exist in both places (e.g. react-native-screens).
config.resolver.nodeModulesPaths = [
  path.resolve(projectRoot, 'node_modules'),
  path.resolve(workspaceRoot, 'node_modules'),
];

module.exports = withNativeWind(config, { input: './global.css' });
