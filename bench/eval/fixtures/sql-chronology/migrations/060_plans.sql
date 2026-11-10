ALTER TABLE public.plans DROP CONSTRAINT plans_tier_check;
ALTER TABLE public.plans ADD CONSTRAINT plans_tier_check CHECK (tier IN ('free', 'pro', 'enterprise'));
