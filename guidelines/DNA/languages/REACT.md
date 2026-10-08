# React Frontend — Usage Patterns

## Types & Client
- Generate TS types from OpenAPI (e.g., `openapi-typescript`)
- Use TanStack Query; compose a thin `fetchJson` with auth and error parsing

```ts
import { getAuthToken } from './auth'; // Example auth token provider

export class ApiError extends Error {
  problem: Record<string, any>;
  status: number;

  constructor(message: string, status: number, problem: Record<string, any>) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
    this.problem = problem;
  }
}

export async function fetchJson<T>(url: string, init: RequestInit = {}): Promise<T> {
  const token = getAuthToken(); // Assume a function that retrieves the bearer token
  const res = await fetch(url, {
    ...init,
    headers: {
      'Accept': 'application/json',
      'Content-Type': 'application/json; charset=utf-8',
      ...(token && { 'Authorization': `Bearer ${token}` }),
      ...(init.headers || {}),
    },
    credentials: 'omit',
  });

  if (!res.ok) {
    const problem = await res.json().catch(() => ({ title: res.statusText }));
    throw new ApiError(problem.title || 'An API error occurred', res.status, problem);
  }

  // Handle 204 No Content
  if (res.status === 204) {
    return undefined as T;
  }

  return res.json();
}
```

## Query Example
```ts
import { useQuery } from '@tanstack/react-query';

type Ticket = {
  id: string;
  title: string;
  priority: 'low' | 'medium' | 'high';
  status: 'open' | 'in_progress' | 'resolved' | 'closed';
  created_at: string;
};

type ListResponse<T> = {
  items: T[];
  page_info?: {
    limit: number;
    next_cursor?: string;
    prev_cursor?: string;
  };
};

export function useTickets(params: { limit?: number; cursor?: string }) {
  const qs = new URLSearchParams();
  if (params.limit) qs.set('limit', String(params.limit));
  if (params.cursor) qs.set('cursor', params.cursor);
  return useQuery({
    queryKey: ['tickets', params],
    queryFn: () => fetchJson<ListResponse<Ticket>>(`/v1/tickets?${qs.toString()}`),
    staleTime: 30_000,
  });
}
```

## Concurrency Example (If-Match)
```ts
export async function updateTicket(id: string, patch: Partial<Ticket>, etag: string) {
  return fetchJson(`/v1/tickets/${id}`, {
    method: 'PATCH',
    headers: { 'If-Match': etag },
    body: JSON.stringify(patch),
  });
}
```
