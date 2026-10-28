import { describe, expect, it } from 'vitest';
import { buildChangePatch, buildNewArtifactPatch, deploymentFields, equalJson, hasUnsafeChangeNumber, resolveSchema, setChangeValue } from './ChangeForm';

const schema = {
  type: 'object',
  properties: {
    deployments: { type: 'array', items: { $ref: '#/$defs/Deployment' } }
  },
  $defs: {
    Deployment: {
      type: 'object',
      required: ['name', 'image'],
      properties: {
        name: { type: 'string' },
        image: { type: 'string', description: 'Serving image' },
        replicas: { type: 'integer', minimum: 1 },
        features: { $ref: '#/$defs/Features' }
      }
    },
    Features: {
      type: 'object',
      required: ['prefixCaching'],
      properties: {
        prefixCaching: { type: 'boolean' },
        toolParser: { type: 'string' }
      }
    }
  }
};

function service() {
  return {
    name: 'example',
    namespace: 'inference',
    deployments: [
      { name: 'primary', image: 'registry.example/engine:old', features: { prefixCaching: true, custom: 'preserve' } },
      { name: 'secondary', image: 'registry.example/engine:other', hardware: 'gpu-b' }
    ],
    future_field: { keep: true }
  };
}

describe('schema-driven deployment changes', () => {
  it('derives fields and constraints from local schema references without inserting defaults', () => {
    const source = service();
    const fields = deploymentFields(schema, source, 0);
    expect(fields.find(field => field.path.join('.') === 'deployments.0.image')).toMatchObject({
      kind: 'string', value: 'registry.example/engine:old', present: true, required: true, description: 'Serving image'
    });
    expect(fields.find(field => field.path.join('.') === 'deployments.0.replicas')).toMatchObject({
      kind: 'integer', present: false, required: false, minimum: 1
    });
    expect(fields.find(field => field.path.join('.') === 'deployments.0.features.prefixCaching')).toMatchObject({
      kind: 'boolean', value: true, present: true, required: true
    });
    expect(source.deployments[0]).not.toHaveProperty('replicas');
  });

  it('resolves a Service root reference and nested deployment references', () => {
    const rooted = {
      $ref: '#/$defs/Service',
      $defs: {
        ...schema.$defs,
        Service: { type: 'object', properties: schema.properties }
      }
    };
    const fields = deploymentFields(rooted, service(), 0);
    expect(fields.find(field => field.path.join('.') === 'deployments.0.image')).toMatchObject({
      kind: 'string', value: 'registry.example/engine:old', required: true
    });
  });

  it('does not materialize KCL expression defaults even when export marks them required', () => {
    const defaultedSchema = {
      ...schema,
      $defs: {
        ...schema.$defs,
        Deployment: {
          ...schema.$defs.Deployment,
          required: ['name', 'image', 'replicas', 'features'],
          properties: {
            ...schema.$defs.Deployment.properties,
            replicas: { type: 'integer', default: '1' },
            features: { $ref: '#/$defs/Features', default: 'Features {}' }
          }
        },
        Features: {
          ...schema.$defs.Features,
          properties: {
            ...schema.$defs.Features.properties,
            prefixCaching: { type: 'boolean', default: 'True' }
          }
        }
      }
    };
    const source = service();
    const fields = deploymentFields(defaultedSchema, source, 1);
    const replicas = fields.find(field => field.path.join('.') === 'deployments.1.replicas');
    const caching = fields.find(field => field.path.join('.') === 'deployments.1.features.prefixCaching');
    expect(replicas).toMatchObject({ kind: 'integer', present: false, required: false });
    expect(caching).toMatchObject({ kind: 'boolean', present: false, required: false });
    expect(replicas?.value).toBeUndefined();
    expect(caching?.value).toBeUndefined();
    expect(source.deployments[1]).not.toHaveProperty('replicas');
    expect(source.deployments[1]).not.toHaveProperty('features');
  });

  it('produces a top-level deployments replacement while preserving every untouched value', () => {
    const source = service();
    const draft = setChangeValue(source, ['deployments', '0', 'image'], 'registry.example/engine:new');
    const patch = buildChangePatch(source, draft);
    expect(patch).toEqual({
      deployments: [
        { name: 'primary', image: 'registry.example/engine:new', features: { prefixCaching: true, custom: 'preserve' } },
        { name: 'secondary', image: 'registry.example/engine:other', hardware: 'gpu-b' }
      ]
    });
    expect(source.deployments[0].image).toBe('registry.example/engine:old');
    expect(patch.deployments).not.toBe(draft.deployments);
    expect(buildChangePatch(source, source)).toEqual({});
  });

  it('binds an explicit immutable artifact to the selected deployment without changing siblings', () => {
    const source = service();
    const image = `registry.example/engine@sha256:${'a'.repeat(64)}`;
    const artifact = {
      name: 'model-v2', uri: `oci://${image}`,
      model_digest: `sha256:${'b'.repeat(64)}`, tokenizer_digest: `sha256:${'c'.repeat(64)}`
    };
    const patch = { namespace: 'changed-namespace' };
    const result = buildNewArtifactPatch(source, patch, 0, artifact);
    expect(result).toEqual({
      namespace: 'changed-namespace',
      deployments: [
        { ...source.deployments[0], artifact: 'model-v2', image, packagedModel: true },
        source.deployments[1]
      ]
    });
    expect(source.deployments[0].image).toBe('registry.example/engine:old');
    expect(patch).toEqual({ namespace: 'changed-namespace' });
  });

  it('rejects non-OCI artifact sources and reordered deployment targets', () => {
    const source = service();
    const artifact = {
      name: 'model-v2', uri: 'oci://registry.example/engine@sha256:abcdef',
      model_digest: 'sha256:model', tokenizer_digest: 'sha256:tokenizer'
    };
    expect(() => buildNewArtifactPatch(source, {}, 0, { ...artifact, uri: 'https://example.test' })).toThrow('OCI URI');
    expect(() => buildNewArtifactPatch(source, { deployments: [...source.deployments].reverse() }, 0, artifact)).toThrow('reordered');
    expect(() => buildNewArtifactPatch(source, {}, 2, artifact)).toThrow('selected deployment');
  });

  it('removes optional nested fields without changing siblings or committed source', () => {
    const source = service();
    const draft = setChangeValue(source, ['deployments', '0', 'features', 'custom'], undefined, true);
    expect(draft.deployments).toEqual([
      { name: 'primary', image: 'registry.example/engine:old', features: { prefixCaching: true } },
      source.deployments[1]
    ]);
    expect(source.deployments[0].features?.custom).toBe('preserve');
    expect(Object.keys(buildChangePatch(source, draft))).toEqual(['deployments']);
  });

  it('prunes only newly-created empty optional objects after undoing an edit', () => {
    const source = service();
    const path = ['deployments', '1', 'features', 'toolParser'];
    const edited = setChangeValue(source, path, 'parser');
    const reverted = setChangeValue(edited, path, undefined, true, source);
    expect(reverted).toEqual(source);
    expect(buildChangePatch(source, reverted)).toEqual({});

    const withEmptyFeatures = {
      ...source,
      deployments: [source.deployments[0], { ...source.deployments[1], features: {} }]
    };
    const editedExisting = setChangeValue(withEmptyFeatures, path, 'parser');
    const revertedExisting = setChangeValue(editedExisting, path, undefined, true, withEmptyFeatures);
    expect(revertedExisting).toEqual(withEmptyFeatures);
  });

  it('rejects service renames, unsupported top-level removal and invalid paths', () => {
    const source = service();
    expect(() => buildChangePatch(source, { ...source, name: 'other' })).toThrow('service name');
    const { namespace: _namespace, ...withoutNamespace } = source;
    expect(() => buildChangePatch(source, withoutNamespace)).toThrow('cannot remove');
    expect(() => setChangeValue(source, ['deployments', '2', 'image'], 'image')).toThrow('array index');
    expect(() => setChangeValue(source, ['namespace', 'nested'], 'value')).toThrow('scalar');
  });

  it('detects unsafe numbers in untouched fields of a replacement deployment array', () => {
    const original = service();
    const source = {
      ...original,
      deployments: [
        original.deployments[0],
        { ...original.deployments[1], opaque: { count: Number.MAX_SAFE_INTEGER + 1 } }
      ]
    };
    const draft = setChangeValue(source, ['deployments', '0', 'image'], 'registry.example/engine:new');
    expect(hasUnsafeChangeNumber(buildChangePatch(source, draft))).toBe(true);
    expect(hasUnsafeChangeNumber({ value: Number.MAX_SAFE_INTEGER, fraction: 0.125 })).toBe(false);
    expect(hasUnsafeChangeNumber({ nested: [Number.POSITIVE_INFINITY] })).toBe(true);
    expect(hasUnsafeChangeNumber({ nested: [Number.NaN] })).toBe(true);
  });

  it('compares JSON independently of object key order but preserves array order', () => {
    expect(equalJson({ a: [1, { x: true, y: null }], b: 2 }, { b: 2, a: [1, { y: null, x: true }] })).toBe(true);
    expect(equalJson([1, 2], [2, 1])).toBe(false);
    expect(equalJson({ x: null }, {})).toBe(false);
  });

  it('resolves escaped local references and rejects external or missing references', () => {
    const root = { $defs: { 'a/b~c': { type: 'string', minLength: 2 } } };
    expect(resolveSchema(root, { $ref: '#/$defs/a~1b~0c' })).toMatchObject({ type: 'string', minLength: 2 });
    expect(() => resolveSchema(root, { $ref: 'https://example.test/schema' })).toThrow('Unsupported schema reference');
    expect(() => resolveSchema(root, { $ref: '#/$defs/missing' })).toThrow('Schema reference not found');
  });
});
