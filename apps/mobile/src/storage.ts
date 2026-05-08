import { Platform } from 'react-native';
import * as SecureStore from 'expo-secure-store';

const TOKEN_KEY = 'sprite_machine_token';
const BASE_URL_KEY = 'sprite_machine_base_url';

/**
 * Same-origin default for web (so the dashboard at https://...fly.dev/app/
 * automatically points the API at https://...fly.dev). On iOS/Android we
 * default to localhost; the user enters their actual URL on Settings.
 */
export const DEFAULT_BASE_URL =
  Platform.OS === 'web' && typeof window !== 'undefined' && window.location?.origin
    ? window.location.origin
    : 'http://localhost:8787';

interface KV {
  get(key: string): Promise<string | null>;
  set(key: string, value: string): Promise<void>;
  del(key: string): Promise<void>;
}

const webKV: KV = {
  async get(key) {
    if (typeof window === 'undefined' || !window.localStorage) return null;
    return window.localStorage.getItem(key);
  },
  async set(key, value) {
    if (typeof window === 'undefined' || !window.localStorage) return;
    window.localStorage.setItem(key, value);
  },
  async del(key) {
    if (typeof window === 'undefined' || !window.localStorage) return;
    window.localStorage.removeItem(key);
  },
};

const nativeKV: KV = {
  get: (key) => SecureStore.getItemAsync(key),
  set: (key, value) => SecureStore.setItemAsync(key, value),
  del: (key) => SecureStore.deleteItemAsync(key),
};

const kv: KV = Platform.OS === 'web' ? webKV : nativeKV;

export async function getToken(): Promise<string | null> {
  return kv.get(TOKEN_KEY);
}

export async function setToken(token: string): Promise<void> {
  await kv.set(TOKEN_KEY, token);
}

export async function getBaseUrl(): Promise<string> {
  const url = await kv.get(BASE_URL_KEY);
  return url ?? DEFAULT_BASE_URL;
}

export async function setBaseUrl(url: string): Promise<void> {
  await kv.set(BASE_URL_KEY, url);
}

export async function clearAll(): Promise<void> {
  await kv.del(TOKEN_KEY);
  await kv.del(BASE_URL_KEY);
}
