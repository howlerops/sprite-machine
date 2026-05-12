const fs = require('node:fs');
const path = require('node:path');
const appJson = require('./app.json');

module.exports = ({ config }) => {
  const expo = {
    ...config,
    ...appJson.expo,
    ios: {
      ...(config.ios ?? {}),
      ...(appJson.expo.ios ?? {}),
    },
    android: {
      ...(config.android ?? {}),
      ...(appJson.expo.android ?? {}),
    },
  };

  if (!fs.existsSync(path.join(__dirname, 'google-services.json'))) {
    delete expo.android.googleServicesFile;
  }

  return expo;
};
