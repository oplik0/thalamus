/**
 * Authentication utilities
 * Token storage using localStorage (web) / expo-secure-store (native)
 */

import { Platform } from "react-native";

const TOKEN_KEY = "thalamus_access_token";
const REFRESH_TOKEN_KEY = "thalamus_refresh_token";

const isWeb = Platform.OS === "web";

export async function getToken(): Promise<string | null> {
  if (!isWeb) {
    try {
      const SecureStore = await import("expo-secure-store");
      return SecureStore.getItemAsync(TOKEN_KEY);
    } catch {
      return null;
    }
  }
  return localStorage.getItem(TOKEN_KEY);
}

export async function setToken(token: string): Promise<void> {
  if (!isWeb) {
    try {
      const SecureStore = await import("expo-secure-store");
      await SecureStore.setItemAsync(TOKEN_KEY, token);
    } catch {
      console.warn("Failed to store token securely");
    }
    return;
  }
  localStorage.setItem(TOKEN_KEY, token);
}

export async function getRefreshToken(): Promise<string | null> {
  if (!isWeb) {
    try {
      const SecureStore = await import("expo-secure-store");
      return SecureStore.getItemAsync(REFRESH_TOKEN_KEY);
    } catch {
      return null;
    }
  }
  return localStorage.getItem(REFRESH_TOKEN_KEY);
}

export async function setRefreshToken(token: string): Promise<void> {
  if (!isWeb) {
    try {
      const SecureStore = await import("expo-secure-store");
      await SecureStore.setItemAsync(REFRESH_TOKEN_KEY, token);
    } catch {
      console.warn("Failed to store refresh token securely");
    }
    return;
  }
  localStorage.setItem(REFRESH_TOKEN_KEY, token);
}

export async function clearToken(): Promise<void> {
  if (!isWeb) {
    try {
      const SecureStore = await import("expo-secure-store");
      await SecureStore.deleteItemAsync(TOKEN_KEY);
      await SecureStore.deleteItemAsync(REFRESH_TOKEN_KEY);
    } catch {
      // Ignore errors
    }
    return;
  }
  localStorage.removeItem(TOKEN_KEY);
  localStorage.removeItem(REFRESH_TOKEN_KEY);
}
