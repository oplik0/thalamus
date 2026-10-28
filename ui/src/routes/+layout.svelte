<script lang="ts">
  import '../app.css';
  import { page } from '$app/state';
  let { children } = $props();
  const links = [ ['/', 'Services'], ['/changes', 'Changes'], ['/runs', 'Runs'], ['/projects', 'Projects'], ['/usage', 'Usage'], ['/keys', 'API keys'], ['/client-setup', 'Client setup'] ];
</script>
<svelte:head><title>Thalamus</title><meta name="description" content="Inference operations control plane" /></svelte:head>
<div class="shell">
  <aside class="sidebar">
    <a class="brand" href="/"><span class="brand-mark">T</span><span>Thalamus<small>Control plane</small></span></a>
    <nav aria-label="Main navigation">
      {#each links as [href, label]}
        <a href={href} aria-current={page.url.pathname === href || (href !== '/' && page.url.pathname.startsWith(href + '/')) ? 'page' : undefined}>{label}</a>
      {/each}
    </nav>
    <div class="sidebar-foot"><span>Inference operations</span><a href="/auth/login">Sign in</a><form method="POST" action="/auth/logout"><button class="logout" type="submit">Sign out</button></form></div>
  </aside>
  <main id="main">{@render children()}</main>
</div>
