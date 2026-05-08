import { createContext, useContext } from 'react';
import { DEFAULT_BASE_URL } from './storage';

export interface AppConfig {
  token: string | null;
  baseUrl: string;
  setToken: (token: string | null) => void;
  setBaseUrl: (url: string) => void;
}

export const AppConfigContext = createContext<AppConfig>({
  token: null,
  baseUrl: DEFAULT_BASE_URL,
  setToken: () => undefined,
  setBaseUrl: () => undefined,
});

export function useAppConfig(): AppConfig {
  return useContext(AppConfigContext);
}
