import { useEffect } from 'react';
import Constants from 'expo-constants';
import { Platform } from 'react-native';
import { registerPushToken } from './api';
import { useAppConfig } from './config-store';

type NotificationsModule = typeof import('expo-notifications');

let notificationsPromise: Promise<NotificationsModule | null> | null = null;

function isExpoGo(): boolean {
  return Constants.appOwnership === 'expo';
}

async function getNotifications(): Promise<NotificationsModule | null> {
  // Expo SDK 53 intentionally limits expo-notifications inside Expo Go and
  // logs noisy warnings on module import. Only load it in development builds /
  // standalone apps, where push notifications are actually supported.
  if (isExpoGo()) return null;
  notificationsPromise ??= import('expo-notifications').then((Notifications) => {
    Notifications.setNotificationHandler({
      handleNotification: async () => ({
        shouldShowAlert: true,
        shouldShowBanner: true,
        shouldShowList: true,
        shouldPlaySound: true,
        shouldSetBadge: true,
      }),
    });
    return Notifications;
  });
  return notificationsPromise;
}

export async function requestAndRegisterPushToken(
  baseUrl: string,
  token: string | null,
  label?: string,
): Promise<string> {
  const Notifications = await getNotifications();
  if (!Notifications) {
    throw new Error('Push notifications require a development build or installed app, not Expo Go.');
  }

  if (Platform.OS === 'android') {
    await Notifications.setNotificationChannelAsync('default', {
      name: 'default',
      importance: Notifications.AndroidImportance.MAX,
      vibrationPattern: [0, 250, 250, 250],
    });
  }

  const { status: existingStatus } = await Notifications.getPermissionsAsync();
  let finalStatus = existingStatus;
  if (existingStatus !== 'granted') {
    const { status } = await Notifications.requestPermissionsAsync();
    finalStatus = status;
  }

  if (finalStatus !== 'granted') {
    throw new Error('Push notification permission not granted');
  }

  const projectId =
    Constants.expoConfig?.extra?.eas?.projectId ?? Constants.easConfig?.projectId ?? '';

  const tokenResponse = await Notifications.getExpoPushTokenAsync(
    projectId ? { projectId } : undefined,
  );
  const pushToken = tokenResponse.data;

  await registerPushToken(baseUrl, token, pushToken, label);
  return pushToken;
}

export function usePushRegistration(): void {
  const { baseUrl, token } = useAppConfig();

  useEffect(() => {
    if (!token || isExpoGo()) return;
    let mounted = true;
    let sub: { remove?: () => void } | null = null;
    void getNotifications().then((Notifications) => {
      if (!mounted || !Notifications) return;
      sub = Notifications.addNotificationReceivedListener((_notification) => {
        // Notifications handled by the handler above
      });
    });
    return () => {
      mounted = false;
      sub?.remove?.();
    };
  }, [token, baseUrl]);
}
