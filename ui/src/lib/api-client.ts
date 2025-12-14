/**
 * API client for communicating with the Thalmus Rust backend.
 *
 * Uses the native fetch API — no need for Axios in modern runtimes.
 * All requests are typed and errors are handled consistently.
 */

const API_BASE_URL = process.env.EXPO_PUBLIC_API_URL ?? "http://localhost:3000";

export class ApiError extends Error {
	constructor(
		public status: number,
		public statusText: string,
		public body?: unknown,
	) {
		super(`API Error ${status}: ${statusText}`);
		this.name = "ApiError";
	}
}

async function handleResponse<T>(response: Response): Promise<T> {
	if (!response.ok) {
		const body = await response.text().catch(() => undefined);
		let parsed: unknown;
		try {
			parsed = body ? JSON.parse(body) : undefined;
		} catch {
			parsed = body;
		}
		throw new ApiError(response.status, response.statusText, parsed);
	}
	return response.json() as Promise<T>;
}

export const apiClient = {
	async get<T>(path: string, init?: RequestInit): Promise<T> {
		const response = await fetch(`${API_BASE_URL}${path}`, {
			...init,
			method: "GET",
			headers: {
				"Content-Type": "application/json",
				...init?.headers,
			},
		});
		return handleResponse<T>(response);
	},

	async post<T>(path: string, body?: unknown, init?: RequestInit): Promise<T> {
		const response = await fetch(`${API_BASE_URL}${path}`, {
			...init,
			method: "POST",
			headers: {
				"Content-Type": "application/json",
				...init?.headers,
			},
			body: body ? JSON.stringify(body) : undefined,
		});
		return handleResponse<T>(response);
	},

	async put<T>(path: string, body?: unknown, init?: RequestInit): Promise<T> {
		const response = await fetch(`${API_BASE_URL}${path}`, {
			...init,
			method: "PUT",
			headers: {
				"Content-Type": "application/json",
				...init?.headers,
			},
			body: body ? JSON.stringify(body) : undefined,
		});
		return handleResponse<T>(response);
	},

	async delete<T>(path: string, init?: RequestInit): Promise<T> {
		const response = await fetch(`${API_BASE_URL}${path}`, {
			...init,
			method: "DELETE",
			headers: {
				"Content-Type": "application/json",
				...init?.headers,
			},
		});
		return handleResponse<T>(response);
	},
};
