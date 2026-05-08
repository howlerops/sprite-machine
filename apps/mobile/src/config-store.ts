import { createContext, useContext } from 'react';
import { DEFAULT_BASE_URL, type ThemeOverride } from './storage';

export interface AppConfig {
  token: string | null;
  baseUrl: string;
  themeOverride: ThemeOverride;
  setToken: (token: string | null) => void;
  setBaseUrl: (url: string) => void;
  setThemeOverride: (value: ThemeOverride) => void;
}

export const AppConfigContext = createContext<AppConfig>({
  token: null,
  baseUrl: DEFAULT_BASE_URL,
  themeOverride: 'system',
  setToken: () => undefined,
  setBaseUrl: () => undefined,
  setThemeOverride: () => undefined,
});

export function useAppConfig(): AppConfig {
  return useContext(AppConfigContext);
}
